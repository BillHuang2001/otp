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

//! Contract A.9 (**self-hosted build**) — the **chain**: compile a real module **in process**,
//! put its output directory on the code path, load the freshly produced `.beam` and **run** it
//! — all inside **one** `beam_rt` invocation per test, cross-checked against the live system
//! Erlang oracle.
//!
//! # What each test proves
//!
//! | Test | The chain, in one invocation |
//! |---|---|
//! | [`b_a_real_stdlib_module_compiles_on_the_code_path_and_runs_in_one_invocation`] | compile `lib/stdlib/src/ordsets.erl` → `code:add_patha/1` → `code:load_file/1` → real `ordsets` calls; **plus** a module only that directory has, loaded and called from it |
//! | [`c_a_dependent_module_calls_the_freshly_compiled_base_in_one_invocation`] | compile a base whose name exists **nowhere else** → `code:add_patha/1` → compile the module that **calls** it → load both → run the caller |
//!
//! Both tests are **non-vacuous** on purpose:
//!
//! * The base module of the (c) chain (`a9_chain_base`) exists on no code-path directory, which
//!   the probe itself asserts (`code:which/1` = `non_existing` **before** the path is added), so
//!   the value the caller returns can only come from the artifact this invocation compiled.
//! * The (b) probe records `code:load_file/1`'s own answer for the real stdlib module — which is
//!   the sticky refusal, because a stdlib module is *already loaded and stick-marked* at boot on
//!   **both** runtimes. That is the oracle's answer too (asserted, not glossed over); the
//!   "directory's output is really used" half is carried by the directory-only module the same
//!   invocation compiles, loads and calls.
//!
//! # Why the artifact comparison is a *value* and not just a file
//!
//! A compile that wrote a plausible-looking-but-wrong `.beam` would still land a file, and a
//! loader that ignored the added directory would still answer `{module, …}` from the installed
//! copy. So each assertion is the **value the freshly produced code computes**, extracted from
//! the invocation's stdout between an explicit `…|` marker and its `|…END` terminator (the `~p`
//! printer wraps long terms, so the marker pair — rather than the whole stdout — is what makes
//! the comparison unambiguous). A missing toolchain, or a checkout with no
//! `lib/stdlib/src/ordsets.erl`, is a **loud skip**, never a silent pass.
//!
//! # Route and cost
//!
//! Each test is a single `beam_rt -noshell -eval '<chain>'` invocation on the **pumping** root
//! route (the in-process compile needs the nested scheduler pump; see
//! `src/context/pump.rs`), plus one oracle invocation of the same probe with its **own** output
//! directory. `ordsets.erl` is the cheap end of the real-module range (~1.5–2.5 s under the
//! debug runtime; `calendar.erl` ≈ 6.6 s and `lists.erl` ≈ 21 s are deliberately **not** used
//! here — the in-crate `src/context/pump.rs` test covers them). Both probes stay well inside
//! [`support::PROBE_TIMEOUT`] (30 s).
//!
//! The two source files the (c) chain compiles are written by the test into `$TMPDIR` — test
//! code, not committed fixtures — and no artifact ever lands inside the repository.

// The shared harness is compiled in whole, but this suite uses only its bounded runner and its
// toolchain discovery; the unused remainder is expected here — the same shape `tests/io_scan.rs`
// uses.
#[allow(dead_code)]
mod support;

use std::path::{Path, PathBuf};
use std::process::Output;
use std::sync::atomic::{AtomicUsize, Ordering};

/// The executable under test, exactly as the other contract suites drive it.
const BINARY: &str = env!("CARGO_BIN_EXE_beam_rt");

// ---------------------------------------------------------------------------------------
// The stdlib source tree
// ---------------------------------------------------------------------------------------

/// The repository root: `$ERL_TOP` when set, else three levels up from this crate's manifest
/// directory (`<root>/erts_rust/crates/beam_rt`) — the same discovery the in-crate
/// `src/context/pump.rs` test uses.
fn source_root() -> PathBuf {
    match std::env::var_os("ERL_TOP") {
        Some(value) => PathBuf::from(value),
        None => Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.."),
    }
}

/// The real stdlib source the (b) chain compiles, or `None` after a loud skip when this
/// checkout has no source tree (an installed-only or relocated build).
fn ordsets_source(banner: &str) -> Option<PathBuf> {
    let source = source_root().join("lib/stdlib/src/ordsets.erl");
    if source.is_file() {
        return Some(source);
    }
    support::skip_loudly(
        banner,
        &format!("no real stdlib source to compile at {}", source.display()),
    );
    None
}

// ---------------------------------------------------------------------------------------
// Running the two runtimes
// ---------------------------------------------------------------------------------------

/// `<beam_rt> -noshell -eval expr` through the built binary, bounded by
/// [`support::PROBE_TIMEOUT`] so a probe that blocks is killed and reaped rather than hanging
/// the suite.
fn beam_rt_eval(expr: &str) -> Output {
    support::run_bounded(Path::new(BINARY), &["-noshell", "-eval", expr]).unwrap_or_else(|| {
        panic!("`{BINARY} -noshell -eval …` could not be run to completion")
    })
}

/// The oracle's stdout for the same `expr` (`erl -noshell -eval expr -s init stop`), or a
/// panic when the live oracle could not answer — both tests ask for the oracle first, so a
/// missing toolchain has already been skipped loudly.
fn oracle_eval(oracle: &Path, expr: &str) -> String {
    support::oracle_eval(oracle, None, expr)
        .unwrap_or_else(|| panic!("the live oracle could not answer the probe:\n{expr}"))
}

/// The probe's own line, extracted from `stdout` between `start` and `end`.
///
/// The `~p` printer wraps a term longer than its line width onto continuation lines and both
/// runtimes may append their own log reports around the probe's line, so the *marker pair* —
/// not the whole stdout — delimits the value. The **only** layout the wrap introduced is then
/// dropped, by trimming each continuation line's indentation and joining the pieces back:
/// that is exactly the `unwrapped/1` rule the `a9_native_floor.rs` suite uses, and it is safe
/// because **no value compared here contains a space** (`~p` on an atom, tuple, list or small
/// integer prints no whitespace).
fn marked(program: &str, stdout: &str, start: &str, end: &str) -> String {
    let Some(from) = stdout.find(start) else {
        panic!("`{program}` printed no `{start}` marker; stdout:\n{stdout}");
    };
    let rest = &stdout[from + start.len()..];
    let Some(upto) = rest.find(end) else {
        panic!("`{program}` printed no `{end}` terminator; stdout:\n{stdout}");
    };
    rest[..upto].lines().map(str::trim_start).collect()
}

/// Asserts the run exited `0`, was silent on stderr, and its marked value is `expected` — then
/// returns the value for the oracle cross-check.
fn value_of(program: &str, output: &Output, start: &str, end: &str, what: &str) -> String {
    let code = output.status.code().unwrap_or(-1);
    assert_eq!(
        code,
        0,
        "{program}: {what}: the run must exit 0 — stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stderr.is_empty(),
        "{program}: {what}: a successful run must be silent on stderr — {}",
        String::from_utf8_lossy(&output.stderr)
    );
    marked(program, &String::from_utf8_lossy(&output.stdout), start, end)
}

/// Asserts the runtime's value is `expected` **and** that the live oracle's own run of the same
/// probe produced the same value.
///
/// The runtime is asserted first, so a divergence is reported even when no oracle answers; a
/// missing oracle has already been skipped loudly by the caller.
fn agrees_with_the_oracle(
    oracle: &Path,
    ours: &str,
    theirs: &str,
    expected: &str,
    what: &str,
) {
    assert_eq!(ours, expected, "beam_rt: {what} (marked value {ours:?})");
    assert_eq!(
        ours,
        theirs,
        "the system erl ({}) must answer the same value for {what}; it answered {theirs:?}",
        oracle.display()
    );
}

// ---------------------------------------------------------------------------------------
// Scratch directories and Erlang literals
// ---------------------------------------------------------------------------------------

/// A private temporary directory, removed when the test ends.
struct TempDir(PathBuf);

impl TempDir {
    /// Creates `$TMPDIR/beam_rt_selfhost_chain_<pid>_<n>_<label>`, unique per test and per run.
    fn new(label: &str) -> TempDir {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let unique = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "beam_rt_selfhost_chain_{}_{unique}_{label}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("a private temporary directory");
        TempDir(path)
    }

    /// Writes `name`/`contents` into the directory and returns the source path.
    fn write(&self, name: &str, contents: &str) -> PathBuf {
        let path = self.0.join(name);
        std::fs::write(&path, contents).expect("write the source");
        path
    }

    /// The artifact `module.beam` inside the directory.
    fn beam(&self, module: &str) -> PathBuf {
        self.0.join(format!("{module}.beam"))
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The **Erlang string literal** for `path`: the temporary names this suite builds are ASCII,
/// which Rust's `Debug` and Erlang's string syntax quote identically.
fn literal(path: &Path) -> String {
    format!("{:?}", path.to_str().expect("an ASCII temporary path"))
}

/// The option list every compile in this suite uses — the A.9 *E0* list: in-process
/// (`no_spawn_compiler_process`), reproducible (`deterministic`, which strips the path-bearing
/// `CInf`/`Line` fields) and docs-free (`no_docs`, so the compressed `Docs` chunk — the one part
/// of the artifact that can legitimately differ between two builds of the same source — is not
/// written at all).
const E0_OPTIONS: &str = "[no_spawn_compiler_process, deterministic, no_docs, {outdir, %OUT%}]";

/// `compile:file(Source, E0Options)` with the output directory filled in.
fn compile_call(source: &Path, outdir: &Path) -> String {
    format!(
        "compile:file({source}, {options})",
        source = literal(source),
        options = E0_OPTIONS.replace("%OUT%", &literal(outdir))
    )
}

/// Asserts a compile really wrote non-empty object code into `outdir`.
fn assert_wrote_artifact(outdir: &Path, module: &str, what: &str) -> u64 {
    let artifact = outdir.join(format!("{module}.beam"));
    let bytes = std::fs::read(&artifact).unwrap_or_else(|error| {
        panic!(
            "{what}: the compile reported a result but wrote no {}: {error}",
            artifact.display()
        )
    });
    assert!(
        !bytes.is_empty(),
        "{what}: the artifact {} is empty",
        artifact.display()
    );
    bytes.len() as u64
}

// ---------------------------------------------------------------------------------------
// (b) — a real stdlib module, on the code path, in one invocation
// ---------------------------------------------------------------------------------------

/// The marker pair the (b) probe prints its values between.
const B_MARK: &str = "A9B|";
const B_END: &str = "|A9BEND";

/// The directory-only module the (b) probe also compiles, loads **from that directory** and
/// calls. Its name exists nowhere else, so a successful call is proof that the loader read the
/// artifact the **same** invocation produced.
const DIR_PROBE_SRC: &str = r#"%% Generated by `beam_rt/tests/selfhost_chain.rs` (never committed).
-module(a9_chain_dirprobe).
-export([answer/0]).

answer() -> 42.
"#;

/// The (b) probe's expected marked value, re-verified against the system `erl`.
///
/// Field by field: `code:which/1` of the directory-only module before the path is added
/// (`non_existing`); the real stdlib compile's answer; `code:add_patha/1`; the added directory
/// on `code:get_path/0`; `code:load_file(ordsets)`'s answer; the real `ordsets` call; the
/// directory-only compile's answer; its load; whether `code:which/1` now names the artifact
/// **inside the added directory**; the directory-only module's own value.
const B_EXPECTED: &str = "non_existing|{ok,ordsets}|true|true|{error,sticky_directory}|[1,2,3]\
                          |{ok,a9_chain_dirprobe}|{module,a9_chain_dirprobe}|true|42";

/// **The (b) chain.** One `beam_rt` invocation compiles the real `lib/stdlib/src/ordsets.erl`
/// in process, adds its output directory to the code path, loads a module and **runs** real
/// `ordsets` functions — and, in the same invocation, compiles/loads/calls a module that only
/// that directory holds.
///
/// # Why the stdlib load step is the sticky refusal
///
/// `code_server:is_sticky/2` (`lib/kernel/src/code_server.erl:1097`) is
/// `erlang:module_loaded(Mod) andalso <marked sticky>`, and a stdlib module is loaded **and**
/// stick-marked during boot on both runtimes. So `code:load_file(ordsets)` answers
/// `{error,sticky_directory}` on this runtime **and** on the system `erl` — the probe asserts
/// that shared answer rather than a fabricated `{module,ordsets}`. The "the directory's output
/// is really used" half of the chain is therefore carried by the directory-only module, which
/// the same invocation compiles, loads through the mutator and calls: `code:which/1` names the
/// artifact inside the added directory and the call answers its value.
///
/// A failure on that one field means the runtime's **boot load-set** for `ordsets` no longer
/// matches the oracle's (the module would then load from the added directory instead of being
/// refused) — read the value, not just the assertion, before changing anything.
///
/// Measured wall time: ~1.5–2.2 s for the whole test on this host — the whole-process start-up
/// plus the in-process `ordsets.erl` compile, the directory-only compile and both probes — of
/// which the oracle's own invocation is ~0.3 s (host load moves the compile's share; see
/// `src/context/pump.rs`, which records the same spread).
#[test]
fn b_a_real_stdlib_module_compiles_on_the_code_path_and_runs_in_one_invocation() {
    const BANNER: &str = "selfhost_chain";
    let Some(oracle) = support::require_oracle(BANNER) else {
        return;
    };
    let Some(source) = ordsets_source(BANNER) else {
        return;
    };

    // The two output directories the two runtimes compile into (each side keeps its own, so the
    // `code:which/1` equality is a property of *that* side's directory).
    let rust_dir = TempDir::new("b_rust");
    let oracle_dir = TempDir::new("b_oracle");
    let dirprobe = rust_dir.write("a9_chain_dirprobe.erl", DIR_PROBE_SRC);

    // The same probe with the side's own output directory spelled in.
    let probe = |out: &TempDir, src: &Path| {
        format!(
            "W0 = code:which(a9_chain_dirprobe),\n\
             C = {stdlib},\n\
             A = code:add_patha({out_lit}),\n\
             M = lists:member({out_lit}, code:get_path()),\n\
             L = code:load_file(ordsets),\n\
             R = ordsets:to_list(ordsets:add_element(3, ordsets:from_list([1, 2]))),\n\
             C2 = {dirprobe},\n\
             L2 = code:load_file(a9_chain_dirprobe),\n\
             W2 = (code:which(a9_chain_dirprobe) =:= {beam_lit}),\n\
             V2 = a9_chain_dirprobe:answer(),\n\
             io:format(\"{mark}~p|~p|~p|~p|~p|~p|~p|~p|~p|~p{end}~n\", \
             [W0, C, A, M, L, R, C2, L2, W2, V2]),\n\
             halt(0).",
            stdlib = compile_call(src, &out.0),
            dirprobe = compile_call(&dirprobe, &out.0),
            out_lit = literal(&out.0),
            beam_lit = literal(&out.beam("a9_chain_dirprobe")),
            mark = B_MARK,
            end = B_END,
        )
    };

    let ours = value_of(
        "beam_rt",
        &beam_rt_eval(&probe(&rust_dir, &source)),
        B_MARK,
        B_END,
        "the (b) chain",
    );
    let theirs = marked(
        "erl",
        &oracle_eval(&oracle, &probe(&oracle_dir, &source)),
        B_MARK,
        B_END,
    );
    agrees_with_the_oracle(
        &oracle,
        &ours,
        &theirs,
        B_EXPECTED,
        "(b) the stdlib compile/load/run and directory-only load/call chain",
    );

    // The compile really wrote into the directory the loader then read.
    let stdlib_bytes = assert_wrote_artifact(&rust_dir.0, "ordsets", "(b)");
    let probe_bytes = assert_wrote_artifact(&rust_dir.0, "a9_chain_dirprobe", "(b)");
    eprintln!(
        "(b) one invocation: ordsets.erl → {stdlib_bytes} bytes (its load is the sticky \
         refusal, as the oracle's is), a9_chain_dirprobe.beam → {probe_bytes} bytes, loaded \
         and called from the added directory"
    );
}

// ---------------------------------------------------------------------------------------
// (c) — a dependent module on the freshly compiled base, in one invocation
// ---------------------------------------------------------------------------------------

/// The marker pair the (c) probe prints its values between.
const C_MARK: &str = "A9C|";
const C_END: &str = "|A9CEND";

/// The **base** module: its name exists on no code-path directory, so the caller's value can
/// only come from the `.beam` the same invocation compiled. Written by the test into `$TMPDIR`
/// (test code, never a committed fixture).
const BASE_SRC: &str = r#"%% Generated by `beam_rt/tests/selfhost_chain.rs` (never committed).
-module(a9_chain_base).
-export([marker/0]).

marker() -> a9_chain_base_marker_v1.
"#;

/// The **dependent** module: it calls [`BASE_SRC`]'s function, so running it exercises the
/// freshly produced base — a pre-installed module of the same name cannot stand in.
const DEP_SRC: &str = r#"%% Generated by `beam_rt/tests/selfhost_chain.rs` (never committed).
-module(a9_chain_dep).
-export([run/0]).

run() -> {dep_called, a9_chain_base:marker()}.
"#;

/// The (c) probe's expected marked value, re-verified against the system `erl`: the base's name
/// resolves nowhere before the path is added (`non_existing`), both compiles answer
/// `{ok, Module}`, both loads answer `{module, Module}`, the caller returns the base's value,
/// and `code:which/1` names the base artifact inside the added directory.
const C_EXPECTED: &str = "non_existing|{ok,a9_chain_base}|true|{ok,a9_chain_dep}\
                          |{module,a9_chain_base}|{module,a9_chain_dep}\
                          |{dep_called,a9_chain_base_marker_v1}|true";

/// **The (c) chain.** One `beam_rt` invocation compiles a base module whose name exists
/// **nowhere else**, puts the output directory on the code path, compiles a module that
/// **calls** the base, loads both and runs the caller — so the value that comes back proves the
/// dependency was resolved against the artifact this invocation produced.
///
/// The probe records `code:which/1` of the base **before** the path is added (`non_existing`
/// on both runtimes): that is what makes the chain non-vacuous — the base cannot have come from
/// anywhere but the directory the same invocation compiled into. The oracle compiles both
/// modules with the **identical** option list into its own directory, adds that directory,
/// loads and runs the same probe, and the two marked values must be equal.
///
/// Measured wall time: ~1.0–1.4 s for the whole test on this host (two tiny in-process compiles
/// on top of the whole-process start-up, plus the oracle's own invocation).
#[test]
fn c_a_dependent_module_calls_the_freshly_compiled_base_in_one_invocation() {
    const BANNER: &str = "selfhost_chain";
    let Some(oracle) = support::require_oracle(BANNER) else {
        return;
    };

    let rust_dir = TempDir::new("c_rust");
    let oracle_dir = TempDir::new("c_oracle");
    let base = rust_dir.write("a9_chain_base.erl", BASE_SRC);
    let dep = rust_dir.write("a9_chain_dep.erl", DEP_SRC);

    // Compile the base, add the directory, compile the caller, load both, run the caller.
    let probe = |out: &TempDir, base: &Path, dep: &Path| {
        format!(
            "W0 = code:which(a9_chain_base),\n\
             C1 = {base_compile},\n\
             A = code:add_patha({out_lit}),\n\
             C2 = {dep_compile},\n\
             LB = code:load_file(a9_chain_base),\n\
             LD = code:load_file(a9_chain_dep),\n\
             V = a9_chain_dep:run(),\n\
             WB = (code:which(a9_chain_base) =:= {beam_lit}),\n\
             io:format(\"{mark}~p|~p|~p|~p|~p|~p|~p|~p{end}~n\", \
             [W0, C1, A, C2, LB, LD, V, WB]),\n\
             halt(0).",
            base_compile = compile_call(base, &out.0),
            dep_compile = compile_call(dep, &out.0),
            out_lit = literal(&out.0),
            beam_lit = literal(&out.beam("a9_chain_base")),
            mark = C_MARK,
            end = C_END,
        )
    };

    let ours = value_of(
        "beam_rt",
        &beam_rt_eval(&probe(&rust_dir, &base, &dep)),
        C_MARK,
        C_END,
        "the (c) chain",
    );
    let theirs = marked(
        "erl",
        &oracle_eval(&oracle, &probe(&oracle_dir, &base, &dep)),
        C_MARK,
        C_END,
    );
    agrees_with_the_oracle(
        &oracle,
        &ours,
        &theirs,
        C_EXPECTED,
        "(c) the base/derived chain",
    );

    // Both artifacts really landed in the directory the loader read.
    let base_bytes = assert_wrote_artifact(&rust_dir.0, "a9_chain_base", "(c)");
    let dep_bytes = assert_wrote_artifact(&rust_dir.0, "a9_chain_dep", "(c)");
    eprintln!(
        "(c) one invocation: a9_chain_base.beam → {base_bytes} bytes, a9_chain_dep.beam → \
         {dep_bytes} bytes; the caller returned the freshly compiled base's marker"
    );
}
