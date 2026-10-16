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
//! # What is asserted
//!
//! A real stdlib module is compiled **in process** under the built `beam_rt` binary
//! ([`CARGO_BIN_EXE_beam_rt`]) with
//! `[no_spawn_compiler_process,deterministic,no_docs,return_errors,return_warnings,{outdir,D}]`,
//! and the probe's output must (i) contain a `Function: ` line — the compiler's own
//! diagnostic print — and (ii) contain **no** `calling_self` anywhere. It is also asserted
//! against the live oracle that the **source** is compilable, so the rust-side failure is
//! demonstrably a residual rather than a broken input.
//!
//! # The deliberate coupling to an out-of-scope residual
//!
//! The module compiles to `{ok, Module, []}` on the oracle, but **not** yet under this
//! runtime: `lib/stdlib/src/erl_tar.erl` crashes in `beam_core_to_ssa:function/2`
//! (`{crash,core_to_ssa,badarg,…}`) and `lib/stdlib/src/erl_lint.erl` crashes in
//! `beam_ssa_opt:phase/4` (`{crash,beam_ssa_opt,{badmap,…}}`) — both are **separate,
//! out-of-scope defects** in the compiler-runtime surface (a `maps:*` iteration-term
//! mis-rejection owned by `crates/beam_bifs`), *not* io defects. This probe therefore
//! **depends** on one of them still being present: the `Function: ` line only exists
//! because a compiler pass catches a crash. It deliberately does **not** assert that the
//! module compiles. If that residual is ever fixed, this probe must be re-pointed at a
//! module that still crashes a compiler pass; the failure message says so.
//!
//! `erl_tar.erl` is probed rather than `erl_lint.erl` (the other stdlib module that reaches
//! the diagnostic) purely for the probe budget: its in-process compile takes ~5 s against a
//! shared `PROBE_TIMEOUT` of 30 s, where `erl_lint.erl` takes ~23 s — a margin too thin for a
//! loaded CI host.
//!
//! A missing `erl` is a **loud** skip, never a silent pass (`tests/support/mod.rs`).

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
// The probe
// ---------------------------------------------------------------------------------------

/// `compile:file(Source, [no_spawn_compiler_process, deterministic, no_docs, return_errors,
/// return_warnings, {outdir, Dir}])`, then the result — the same option list the oracle is
/// given, so the only difference between the two runs is the runtime.
///
/// `RETSAW` is the probe's own marker so the assertion cannot be satisfied by an unrelated
/// line. `no_docs` keeps a real module's artifact independent of the `Docs` chunk and
/// `return_errors`/`return_warnings` make the compiler **return** the failure instead of
/// exiting, so the probe always reaches its `halt(0)`.
fn compile_probe(src: &Path, outdir: &Path) -> String {
    format!(
        "R = compile:file({}, [no_spawn_compiler_process, deterministic, no_docs, \
         return_errors, return_warnings, {{outdir, {}}}]),\n\
         io:format(\"RETSAW ~p~n\", [R]),\n\
         halt(0).",
        literal(src),
        literal(outdir)
    )
}

/// **The diagnostic-unmasking probe.** See the module docs for what is compared and why the
/// `Function: ` line is coupled to an out-of-scope residual.
#[test]
fn the_compilers_own_diagnostic_is_not_masked_as_calling_self() {
    let Some(oracle) = support::require_oracle("io_diagnostic") else {
        return;
    };
    let src = source_root().join("lib/stdlib/src/erl_tar.erl");
    if !src.is_file() {
        support::skip_loudly(
            "io_diagnostic",
            &format!("no real stdlib source to compile at {}", src.display()),
        );
        return;
    }

    let dir = TempDir::new();
    let rust_out = dir.0.join("rust_out");
    let oracle_out = dir.0.join("oracle_out");
    // `compile` does not create the output directory.
    for out in [&rust_out, &oracle_out] {
        fs::create_dir_all(out).expect("the output directory");
    }

    // ---- the oracle: the source really is compilable ----
    let theirs = oracle_eval(&oracle, &compile_probe(&src, &oracle_out));
    assert_eq!(
        theirs.code, 0,
        "the oracle must complete the probe: exit {} — {}",
        theirs.code,
        theirs.combined()
    );
    assert!(
        theirs.out_str().starts_with("RETSAW {ok,erl_tar,"),
        "the source must be compilable on the oracle (so the rust-side failure is a residual, \
         not a broken input) — the oracle printed {:?}",
        theirs.out_str()
    );

    // ---- this runtime: the compiler's own diagnostic must survive ----
    let ours = beam_rt_eval(&compile_probe(&src, &rust_out));
    assert_eq!(
        ours.code, 0,
        "the probe must reach its own halt(0) — exit {} — {}",
        ours.code,
        ours.combined()
    );
    eprintln!(
        "in-process compile of erl_tar.erl under beam_rt printed:\n{}",
        ours.combined()
    );

    // (ii) the reason must not be the io-device failure. `calling_self` is exactly what the
    // compiler's diagnostic call used to raise, replacing its real reason with its own.
    assert!(
        !ours.combined().contains("calling_self"),
        "the compiler's diagnostic call must not raise `calling_self` — the io device floor \
         serves `io:fwrite/2` natively. Captured output: {:?}",
        ours.combined()
    );

    // (i) the compiler's own `Function: <name>/<arity>` line must have been printed. This
    // line only exists because a compiler pass catches a crash, so the probe is deliberately
    // coupled to that (separate) residual — see the module docs.
    assert!(
        ours.combined().contains("Function: "),
        "the compiler's own `Function: <name>/<arity>` diagnostic must reach the output. If \
         the in-process compile now SUCCEEDS (the `beam_core_to_ssa`/`maps:*` residual was \
         fixed) there is no crash for the handler to report and this probe must be re-pointed \
         at a module that still crashes a compiler pass. Captured output: {:?}",
        ours.combined()
    );
}
