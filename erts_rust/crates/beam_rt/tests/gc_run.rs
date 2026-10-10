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

//! **The GC root-set fix, proved end to end: a real module whose body moves the heap.**
//!
//! `src/context/gc.rs` relocates the *executing machine's* whole root set (every x register,
//! the whole Erlang stack, the whole FR file) at a heap-admission point. This suite is the
//! end-to-end acceptance evidence for the smallest real workload that reaches such a point and
//! then **reads a term across it**: a module calling the stdlib's
//! `erl_error:format_exception/3`, which builds a deep tuple/list structure on the caller's
//! heap (so a collection runs mid-body) and then walks the argument it was handed.
//!
//! The fixture is the exact module the fix was written for:
//!
//! ```erlang
//! -module(w).
//! -export([f9/0]).
//! f9() -> erl_error:format_exception(throw, {1,2}, []).
//! ```
//!
//! It is compiled **at test time** with the system `erlc` (never committed: a `.beam` is
//! compiler-version-dependent output, not source) and run through the runtime's single-pass
//! `Runtime::run_function`, which is the path that reaches the heap-admission point with the
//! machine's whole root set. The rendered `~w` string — the printer `beam_rt run` uses — must
//! equal the **oracle's** rendering of `w:f9()` measured from the same fixture: the two
//! runtimes must agree byte for byte.
//!
//! A missing toolchain is a **loud skip**, never a silent pass (the shared
//! [`support`](crate) module resolves `erl`/`erlc` from the filesystem and bounds every probe).

// The shared helper module is compiled into every integration-test binary, so this binary
// would otherwise warn on the entry points it does not call.
#[allow(dead_code)]
mod support;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

use beam_rt::Runtime;

/// The fixture's module name (also its source-file stem and its compiled `.beam` name).
const MODULE: &str = "w";

/// The exact module body the fix was written for: one exported nullary function whose body
/// calls into stdlib, so the callee allocates on the caller's heap and a collection can run
/// between the build and the read of the argument.
const FIXTURE_ERL: &str = "-module(w).\n\
     -export([f9/0]).\n\
     f9() -> erl_error:format_exception(throw, {1,2}, []).\n";

/// The oracle probe: render `w:f9()`'s value with `~w` — the **same** printer `beam_rt run`
/// renders its result with — so the two strings are directly comparable.
const ORACLE_EVAL: &str =
    "V = w:f9(), io:format(\"~w~n\", [V]), halt(0).";

// ---------------------------------------------------------------------------------------
// Toolchain discovery + the loud skip
// ---------------------------------------------------------------------------------------

/// The compiler: a sibling of the resolved oracle first (the same installation), then `$PATH`.
///
/// The shared [`support`] module resolves the *oracle* only (this suite needs the compiler
/// too), so the one extra resolver lives here rather than being duplicated into the shared
/// module every suite compiles.
fn resolve_erlc() -> Option<PathBuf> {
    if let Some(dir) = support::resolve_oracle().as_deref().and_then(Path::parent) {
        let candidate = dir.join("erlc");
        if support::is_executable(&candidate) {
            return Some(candidate);
        }
    }
    support::find_on_path("erlc")
}

/// Prints the loud skip banner — a missing compiler is never a silent pass.
fn skip_loudly(why: &str) {
    eprintln!();
    eprintln!("=============== beam_rt gc_run SKIPPED ===============");
    eprintln!("{why}: the end-to-end GC root-set comparison cannot run.");
    eprintln!("Set $ERTS_ORACLE (or install Erlang/OTP) and re-run. NOT a pass.");
    eprintln!("=====================================================");
    eprintln!();
}

// ---------------------------------------------------------------------------------------
// The compiled fixture
// ---------------------------------------------------------------------------------------

/// A compiled fixture in its own scratch directory (removed on drop).
struct Fixture {
    /// The scratch directory: a safe single `-pa` for the oracle (no `lists.beam` in it).
    dir: PathBuf,
    /// The compiled `w.beam`.
    beam: PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // The directory is ours and disposable; a cleanup failure is reported, not hidden.
        if let Err(error) = std::fs::remove_dir_all(&self.dir) {
            eprintln!(
                "warning: could not remove the fixture dir {}: {error}",
                self.dir.display()
            );
        }
    }
}

/// Compiles [`FIXTURE_ERL`] with the system `erlc`, or `None` with a loud skip when the
/// toolchain cannot do it.
fn build_fixture(erlc: &Path) -> Option<Fixture> {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("beam_rt_gc_run_{}_{n}", std::process::id()));
    if let Err(error) = std::fs::create_dir_all(&dir) {
        skip_loudly(&format!("cannot prepare {}: {error}", dir.display()));
        return None;
    }
    let source = dir.join(format!("{MODULE}.erl"));
    if let Err(error) = std::fs::write(&source, FIXTURE_ERL) {
        skip_loudly(&format!("cannot write {}: {error}", source.display()));
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
            skip_loudly(&format!("cannot run `erlc`: {error}"));
            return None;
        }
    };
    if !output.status.success() {
        skip_loudly(&format!(
            "`erlc` exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ));
        return None;
    }
    let beam = dir.join(format!("{MODULE}.beam"));
    if !beam.is_file() {
        skip_loudly(&format!("`erlc` produced no {}", beam.display()));
        return None;
    }
    Some(Fixture { dir, beam })
}

// ---------------------------------------------------------------------------------------
// The test
// ---------------------------------------------------------------------------------------

/// **The fix, end to end**: the runtime runs a real module whose body builds on its heap and
/// then reads the argument across that build, and its `~w` rendering of the result is
/// byte-identical to the oracle's.
///
/// A regression that left a live x register (or the FR file) un-relocated at the heap-admission
/// point inside `erl_error:format_exception/3` shows up here as a wrong or garbled rendering —
/// never as a silently-passing test.
#[test]
fn the_runtime_renders_the_allocating_module_exactly_like_the_oracle() {
    let Some(oracle) = support::require_oracle("gc_run") else {
        return;
    };
    let Some(erlc) = resolve_erlc() else {
        skip_loudly("no `erlc` available (and none beside $ERTS_ORACLE)");
        return;
    };
    let Some(fixture) = build_fixture(&erlc) else {
        return;
    };

    // The oracle's `~w` rendering of `w:f9()`, measured from the same fixture (the `-pa`
    // puts this scratch dir first, and it holds only `w.beam`).
    let Some(expected) = support::oracle_eval(&oracle, Some(&fixture.dir), ORACLE_EVAL) else {
        skip_loudly("the oracle probe could not answer for the fixture");
        return;
    };
    assert!(
        !expected.is_empty(),
        "the oracle rendered nothing for w:f9()"
    );

    let mut rt = Runtime::with_defaults().expect("the default runtime");
    let value = rt
        .run_function(
            fixture.beam.to_str().expect("the fixture path is UTF-8"),
            "f9",
            0,
            &[],
        )
        .expect("w:f9() runs on the runtime");
    // `format_term_w` is the `~w` printer `beam_rt run` uses, so this is the exact string the
    // CLI would print.
    let rendered = rt
        .format_term_w(value, None)
        .expect("the result is printable");
    eprintln!("gc_run: runtime rendered {rendered:?}; oracle rendered {expected:?}");
    assert_eq!(
        rendered.trim(),
        expected.trim(),
        "the runtime's ~w rendering of w:f9() must equal the oracle's"
    );
}
