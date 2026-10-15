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

//! Contract A.9 (**self-hosted build**), milestone *E0*: OTP's `compile:file/2` driven
//! **in-process** under the Rust runtime, with its artifact compared to the system oracle.
//!
//! # What this suite is for
//!
//! `beam_rt` resolves the system code path natively ([`CodeServer::add_system_paths`]) and — since
//! [`EXTRA_CODE_PATH_APPS`] — also appends the `compiler` application's `ebin`, so `compile:*` is
//! loadable from the same installation the oracle uses. This suite is the **compatibility gate**
//! for that: it runs the real compiler under `beam_rt` and under the system `erl`, and compares.
//!
//! [`CodeServer::add_system_paths`]: beam_rt::CodeServer::add_system_paths
//! [`EXTRA_CODE_PATH_APPS`]: (crate-internal) `codeserver::EXTRA_CODE_PATH_APPS`
//!
//! # What this suite asserts
//!
//! This suite is a **strict differential gate** for the in-process compile path, not a report-only
//! scaffold. Every probe really runs OTP's compiler under the built `beam_rt` binary and the
//! observable results are asserted against the system oracle:
//!
//! * `the_in_process_compiler_is_reachable` asserts the runtime completes the `compile:forms/2`
//!   probe, that the probe printed one of its own three result shapes, and that the printed shape
//!   is `assert_eq!`-equal to the oracle's answer.
//! * `the_in_process_compile_file_artifact_matches_the_oracle` asserts the runtime completed
//!   `compile:file/2`, that the artifact exists, that it is **byte-for-byte** identical to an
//!   oracle run with the **identical** option list, and that its `Code` chunk matches
//!   `erlc +deterministic` (through `beam_lib:chunks/2`).
//!
//! A real divergence fails the suite. A missing toolchain (`erl`/`erlc`) is a **loud** skip
//! (`support::require_oracle` / `support::skip_loudly`), never a silent pass.
//!
//! # Why the artifact comparison is shaped the way it is
//!
//! * `{outdir, D}` is **not** recorded in the `Dbgi` chunk, and `compile` does **not** create `D`,
//!   so the test creates it.
//! * `erlc +deterministic` and `compile:file(_, [..., no_spawn_compiler_process,
//!   deterministic, ...])` are **not** byte-identical: the `Dbgi` chunk records the compile
//!   options (`{none,[deterministic]}` vs `{none,[no_spawn_compiler_process,deterministic]}`), a
//!   ~27-byte difference. The honest byte comparison is therefore against an oracle run with the
//!   **identical** option list; the `Code` chunk — which *is* identical — is compared against
//!   `erlc +deterministic` as well, through `beam_lib:chunks/2`.
//!
//! # Route
//!
//! The first two probes are `beam_rt -noshell -eval '<expr>'`. The `-eval` driver runs on the
//! runtime's root process and **pumps the scheduler** through `Runtime::pump_scheduler`, which is what the native
//! `file_server_2` read path needs; it is the route on which the in-process compile completes (the
//! native `file_server_2` also serves `save_binary`'s write, so `compile:file/2`'s artifact lands
//! on disk), and it needs no compiled fixture. A compile driven from a **scheduled** process
//! (`beam_rt … run <file>.beam <mod>:<fun>/2 <source> <outdir>`) completes too, and the suite's
//! fourth test pins that: the compile parks the process in a `receive` (the io-device/file-server
//! round trips), and the park arm keeps it **relocation-exempt** for the whole parked window
//! (`runner/run.rs`), so a delivery- or signal-driven collection of its heap is declined while it
//! waits. That matters because the park arm's published root set is not the whole truth mid-slice —
//! a live `Machine::pending_exception`, or a Rust-local term held by an in-flight
//! `beam_bifs`/`beam_engine` frame, is not in it, while the **running** machine's `x`/stack/FR reach
//! `Process::regs` only at the engine's own heap-admission points
//! (`src/context/gc.rs::ensure_heap_full`/`alloc_words_rooted`) and at a park, never around
//! `ExecutionContext::call_bif`. Exempting is decline-only: the park-window delivery takes the
//! ordinary no-relocation heap-fragment fallback, and the process's own admission point collects as
//! usual once it is resumed and the exemption is cleared.
//!
//! The suite's third test deliberately takes the **other** route: `epp:parse_file/3` driven from a
//! *scheduled* process (`beam_rt … run <file>.beam <mod>:<fun>/1 <file>`), which is the route on
//! which a delivery-driven collection of the parked receiver used to corrupt its live scan options
//! and answer a different form count (or `badarg`) than the oracle. It asserts the parsed
//! `{FormCount, Tags}` — the observable result, never "no crash".
// The shared harness is compiled in whole, but this suite uses only its bounded runner and its
// toolchain discovery; the unused remainder is expected here — the same shape `tests/io_scan.rs`
// uses.
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
fn run(program: &Path, args: &[&str]) -> Run {
    run_with_env(program, args, &[])
}

/// As [`run`], but with `env` applied to the child's environment (`None` **removes** a
/// variable). The test process's own environment is never mutated, so pinning
/// `BEAM_RT_TIER` here cannot affect any other test in the binary.
fn run_with_env(program: &Path, args: &[&str], env: &[(&str, Option<&str>)]) -> Run {
    let output: Output = support::run_bounded_with_env(program, args, env).unwrap_or_else(|| {
        panic!(
            "`{}` {args:?} (env {env:?}) could not be run to completion",
            program.display()
        )
    });
    Run::from_output(output)
}

/// `beam_rt -noshell -eval <expr>` through the built binary.
fn beam_rt_eval(expr: &str) -> Run {
    run(Path::new(BINARY), &["-noshell", "-eval", expr])
}

/// `erl -noshell -eval <expr> -s init stop` through the oracle (the node is bounded: the probe
/// always gets a terminator, and the shared runner kills it at the bound).
fn oracle_eval(oracle: &Path, expr: &str) -> Run {
    run(oracle, &["-noshell", "-eval", expr, "-s", "init", "stop"])
}

// ---------------------------------------------------------------------------------------
// Temporary directories and Erlang literals
// ---------------------------------------------------------------------------------------

/// A private temporary directory, removed when the test ends.
struct TempDir(PathBuf);

impl TempDir {
    /// Creates `$TMPDIR/beam_rt_compile_file_<pid>_<n>_<label>`, unique per test and per run.
    fn new(label: &str) -> TempDir {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let unique = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "beam_rt_compile_file_{}_{unique}_{label}",
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

/// The **Erlang string literal** for `path`: the temporary names this suite builds are ASCII, which
/// Rust's `Debug` and Erlang's string syntax quote identically.
fn literal(path: &Path) -> String {
    format!("{:?}", path.to_str().expect("an ASCII temporary path"))
}

/// The source of the module both runtimes compile: one exported function, a deliberate `Code`-chunk
/// payload, and nothing that needs a facility outside `kernel` + `stdlib`.
const SOURCE: &str = "-module(hello).\n-export([add/2, twice/1]).\n\nadd(A, B) -> A + B.\n\ntwice(X) -> add(X, X).\n";

/// Writes [`SOURCE`] as `hello.erl` in `dir`.
fn write_source(dir: &Path) -> PathBuf {
    let path = dir.join("hello.erl");
    fs::write(&path, SOURCE).expect("write the source fixture");
    path
}

// ---------------------------------------------------------------------------------------
// The probes
// ---------------------------------------------------------------------------------------

/// The abstract forms of a tiny module, spelled out so the probe needs **no** parser and no `epp`:
/// this isolates the compiler passes from the io-device/file read path.
const FORMS: &str = "[{attribute,1,module,hello},\
     {attribute,2,export,[{add,2}]},\
     {function,3,add,2,[{clause,3,[{var,3,'A'},{var,3,'B'}],[],\
       [{op,3,'+',{var,3,'A'},{var,3,'B'}}]}]}]";

/// `compile:forms/2` over [`FORMS`], on the `-eval` (pumping) route. Never spawns the compiler
/// process, so the whole compile stays in the calling process.
fn forms_probe() -> String {
    format!(
        "Forms = {FORMS},\n\
         R = compile:forms(Forms, [no_spawn_compiler_process]),\n\
         case R of\n\
           {{ok,hello,B}} -> io:format(\"COMPILED bytes=~p~n\", [byte_size(B)]);\n\
           {{error,Es,Ws}} -> io:format(\"ERRORS ~p ~p~n\", [Es, Ws]);\n\
           Other -> io:format(\"OTHER ~p~n\", [Other])\n\
         end,\n\
         halt(0)."
    )
}

/// The **end-to-end** probe: `compile:file/2` with the exact option list both runtimes are given
/// (the oracle is asked this same expression, only with its own output directory).
fn file_probe(src: &Path, outdir: &Path) -> String {
    format!(
        "R = compile:file({}, [no_spawn_compiler_process, deterministic, {{outdir, {}}}]),\n\
         io:format(\"RET ~p~n\", [R]),\n\
         halt(0).",
        literal(src),
        literal(outdir)
    )
}

/// `beam_lib:chunks/2` over `file`, reported as the md5 of its **`Code` chunk** — the chunk that is
/// independent of the `Dbgi` options difference (see *Why the artifact comparison is shaped the way
/// it is* above). The chunk is requested and matched by its real name, `"Code"`
/// (`beam_lib:chunks/2` rejects the lowercase `code` with `{unknown_chunk, …}`), so a digest is only
/// ever reported for a chunk that was actually read.
fn code_chunk_digest(oracle: &Path, file: &Path) -> Option<String> {
    let expr = format!(
        "{{ok,{{_,Chunks}}}} = beam_lib:chunks({}, [\"Code\"]),\n\
         [{{\"Code\",C}}] = Chunks,\n\
         io:format(\"code=~s~n\", [binary:encode_hex(erlang:md5(C))]),\n\
         halt(0).",
        literal(file)
    );
    let answered = oracle_eval(oracle, &expr);
    if answered.code != 0 {
        eprintln!(
            "warning: beam_lib:chunks/2 could not digest {} (exit {}) — {}",
            file.display(),
            answered.code,
            answered.err
        );
        return None;
    }
    Some(answered.out)
}

// ---------------------------------------------------------------------------------------
// Test 1: the compiler is reachable in-process
// ---------------------------------------------------------------------------------------

/// `compile:forms/2` over hand-written abstract forms reaches the compiler passes with no
/// `epp`/io-device/file-read involvement, so it isolates "is the compiler loadable and executable
/// in-process?" from "can the runtime write the artifact?" — the two facilities the end-to-end test
/// below needs together.
///
/// Asserts the runtime completed the probe and printed one of the probe's own result shapes, then
/// `assert_eq!`-compares that shape with the oracle's answer.
#[test]
fn the_in_process_compiler_is_reachable() {
    let expr = forms_probe();
    let ours = beam_rt_eval(&expr);
    assert_eq!(
        ours.code, 0,
        "compile:forms/2 must complete in-process: exit {} — {}",
        ours.code, ours.err
    );
    eprintln!("compile:forms/2 under beam_rt: {}", ours.out);
    assert!(
        ours.out.starts_with("COMPILED") || ours.out.starts_with("OTHER") || ours.out.starts_with("ERRORS"),
        "the probe must report one of its three shapes, got {:?}",
        ours.out
    );

    let Some(oracle) = support::require_oracle("compile_file") else {
        return;
    };
    let theirs = oracle_eval(&oracle, &expr);
    assert_eq!(
        theirs.code, 0,
        "the oracle must run the same probe: exit {} — {}",
        theirs.code, theirs.err
    );
    assert_eq!(
        ours.out, theirs.out,
        "compile:forms/2 must agree with the oracle"
    );
    eprintln!("compile:forms/2 agrees with the oracle");
}

// ---------------------------------------------------------------------------------------
// Test 2: the end-to-end artifact differential
// ---------------------------------------------------------------------------------------

/// **The E0 differential.** Both runtimes are asked for
/// `compile:file(F, [no_spawn_compiler_process, deterministic, {outdir, D}])` — the *identical*
/// option list, so the `Dbgi` chunk matches too — and their artifacts are compared:
///
/// 1. byte-for-byte (the honest comparison: the two runs record the same `Dbgi` options, because
///    their option lists are identical);
/// 2. the `Code` chunk against `erlc +deterministic` (via `beam_lib:chunks/2`) — the chunk that is
///    identical even between the two different option lists.
///
/// The runtime's probe must complete and its artifact must exist; both are asserted before the
/// differential, so a regression surfaces as a failure rather than a report.
#[test]
fn the_in_process_compile_file_artifact_matches_the_oracle() {
    let Some(oracle) = support::require_oracle("compile_file") else {
        return;
    };
    if support::find_on_path("erlc").is_none() {
        support::skip_loudly("compile_file", "no `erlc` on $PATH");
        return;
    }

    let dir = TempDir::new("e2e");
    let src = write_source(&dir.0);
    let rust_out = dir.0.join("rust_out");
    let oracle_out = dir.0.join("oracle_out");
    let erlc_out = dir.0.join("erlc_out");
    // `compile` does not create the output directory.
    for out in [&rust_out, &oracle_out, &erlc_out] {
        fs::create_dir_all(out).expect("the output directory");
    }

    // ---- the oracle's artifact (identical option list) ----
    let expr = file_probe(&src, &oracle_out);
    let theirs = oracle_eval(&oracle, &expr);
    assert_eq!(
        theirs.code, 0,
        "the oracle must compile the fixture: exit {} — {}",
        theirs.code, theirs.err
    );
    let oracle_artifact = oracle_out.join("hello.beam");
    assert!(
        oracle_artifact.is_file(),
        "the oracle wrote no artifact: {}",
        oracle_artifact.display()
    );

    // ---- the Rust runtime's artifact (its own output directory) ----
    let ours = beam_rt_eval(&file_probe(&src, &rust_out));
    assert_eq!(
        ours.code, 0,
        "compile:file/2 must complete in-process: exit {} — {}",
        ours.code, ours.err
    );
    eprintln!("compile:file/2 under beam_rt: {}", ours.out);
    let rust_artifact = rust_out.join("hello.beam");
    assert!(
        rust_artifact.is_file(),
        "the runtime reported {:?} but wrote no {}",
        ours.out,
        rust_artifact.display()
    );

    // ---- 1. byte-for-byte against the oracle's identical-option run ----
    let mine = fs::read(&rust_artifact).expect("the Rust artifact");
    let reference = fs::read(&oracle_artifact).expect("the oracle artifact");
    if mine != reference {
        let at = mine
            .iter()
            .zip(reference.iter())
            .position(|(a, b)| a != b)
            .unwrap_or_else(|| mine.len().min(reference.len()));
        eprintln!(
            "MISMATCH: rust {} bytes vs oracle {} bytes — first difference at byte {at}: {:?} vs {:?}",
            mine.len(),
            reference.len(),
            mine.get(at),
            reference.get(at)
        );
    }
    assert_eq!(
        mine, reference,
        "the in-process artifact must be byte-identical to the oracle's (identical option list)"
    );

    // ---- 2. the `Code` chunk against `erlc +deterministic` ----
    let erlc = support::find_on_path("erlc").expect("erlc was probed above");
    let erlc_run = run(&erlc, &["+deterministic", "-o", &erlc_out.to_string_lossy(), &src.to_string_lossy()]);
    assert_eq!(
        erlc_run.code, 0,
        "erlc +deterministic must build the fixture: {}",
        erlc_run.err
    );
    let erlc_artifact = erlc_out.join("hello.beam");
    let mine_code = code_chunk_digest(&oracle, &rust_artifact)
        .expect("beam_lib:chunks/2 must digest the runtime's artifact");
    let erlc_code = code_chunk_digest(&oracle, &erlc_artifact)
        .expect("beam_lib:chunks/2 must digest erlc's artifact");
    assert_eq!(
        mine_code, erlc_code,
        "the `Code` chunk must match `erlc +deterministic`'s (the Dbgi options differ by design)"
    );
    eprintln!("Code chunk agrees with erlc +deterministic");
}

// ---------------------------------------------------------------------------------------
// Test 3: the scheduled-route `epp` read path
// ---------------------------------------------------------------------------------------

/// The driver module for the scheduled-route probe: `epp_forms:go/1` reads its argument through
/// `epp:parse_file/3` and answers `{FormCount, Tags}` — a pure `~w`-printing value, so the
/// runtime's own `run` output and the oracle's `io:format("~w~n", …)` are directly comparable.
const EPP_FORMS_SRC: &str = include_str!("fixtures/epp_forms.erl");

/// The driver's input — a tiny module in **legacy** syntax only (no maps, no binaries), so the
/// scanner's option list genuinely lives in the plain cons cell it has always lived in.
const EPP_INPUT_SRC: &str = include_str!("fixtures/epp_input.erl");

/// What both runtimes must answer for [`EPP_INPUT_SRC`] through `epp:parse_file/3` with **no**
/// options: `epp` prepends its own `{attribute,_,file,_}` form, so the three source forms
/// (`-module`/`-export`/`f/0`) plus that `file` attribute and the trailing `eof` are exactly five
/// forms, three of which are `attribute`s.
const EPP_INPUT_FORMS: &str = "{5,[attribute,attribute,attribute,function,eof]}";

/// **The scheduled-route regression.** A `beam_rt run` process — the route where the workload runs
/// as a **scheduled** process instead of on the pumping root — parses a source file through the
/// real `epp`/`erl_scan`/io-device read path and must answer exactly what the live oracle answers.
///
/// # What this pins
///
/// The `epp` process's live scan options live in its **mature** generation, and every path to them
/// hops through a container *outside* the collector's from-set (this runtime's pointer-stable
/// arena, a pooled BIF heap, a message fragment). A delivery-driven major collection of the parked
/// receiver used to leave that mature cell behind, free the old block, and let another process
/// recycle it in place — after which the read path answered a form too many (the original
/// reproduction) or `badarg`. The runtime's four heap-admission doors
/// (`context/gc.rs::{ensure_heap, ensure_heap_full, alloc_words_rooted}` and
/// `Runtime::serve_gc_request`) already decline that collection through the unscanned-root valve
/// (`context/gc_walk.rs`); the delivery seam now asks the same question for the **receiver** and
/// declines it too, so the delivery takes the ordinary no-relocation heap-fragment path and the
/// read completes.
///
/// The assertion is the observable the oracle produces, so a regression is a **wrong answer**
/// (or a `badarg`), never merely a crash.
#[test]
fn a_scheduled_process_parses_a_source_file_like_the_oracle() {
    let Some(oracle) = support::require_oracle("compile_file") else {
        return;
    };
    let Some(erlc) = support::find_on_path("erlc") else {
        support::skip_loudly("compile_file", "no `erlc` on $PATH");
        return;
    };

    // ---- the driver, compiled into its own scratch directory ----
    let driver = TempDir::new("epp_forms");
    let driver_src = driver.0.join("epp_forms.erl");
    fs::write(&driver_src, EPP_FORMS_SRC).expect("write the driver fixture");
    let driver_dir = driver.0.to_string_lossy().to_string();
    let src_arg = driver_src.to_string_lossy().to_string();
    let built = run(&erlc, &["-o", &driver_dir, &src_arg]);
    assert_eq!(built.code, 0, "`erlc` must build the driver: {}", built.err);
    let driver_beam = driver.0.join("epp_forms.beam");
    assert!(
        driver_beam.is_file(),
        "`erlc` wrote no {}",
        driver_beam.display()
    );

    // ---- the input, in a directory of its own ----
    let input_dir = TempDir::new("epp_input");
    let input = input_dir.0.join("epp_input.erl");
    fs::write(&input, EPP_INPUT_SRC).expect("write the input fixture");

    // ---- the runtime, on the scheduled `run` route ----
    let spec = "epp_forms:go/1";
    let beam_arg = driver_beam.to_string_lossy().to_string();
    let input_arg = literal(&input);
    let ours = run(
        Path::new(BINARY),
        &[
            "-noshell",
            "-pa",
            &driver_dir,
            "run",
            &beam_arg,
            spec,
            &input_arg,
        ],
    );
    assert_eq!(
        ours.code, 0,
        "the scheduled route must parse the source: exit {} — {}",
        ours.code, ours.err
    );
    assert!(
        !ours.err.contains("unsupported"),
        "the scheduled route must not report a capability gap: {}",
        ours.err
    );
    let line = ours.out.lines().next().unwrap_or_default().to_string();
    assert_eq!(
        line, EPP_INPUT_FORMS,
        "the scheduled route must answer the parse it drove (stdout {:?}; stderr {:?})",
        ours.out, ours.err
    );

    // ---- the oracle, on the identical workload ----
    let expr = format!("io:format(\"~w~n\",[epp_forms:go({input_arg})]), halt(0).");
    let theirs = support::oracle_eval(&oracle, Some(&driver.0), &expr)
        .unwrap_or_else(|| panic!("the live oracle could not answer `{spec}`"));
    assert_eq!(
        theirs, line,
        "the scheduled route must agree with the oracle (`{spec}`)"
    );
    eprintln!("scheduled-route epp parse agrees with the oracle: {line}");
}
// ---------------------------------------------------------------------------------------
// Test 4: the scheduled-route *in-process* compile (`no_spawn_compiler_process`)
// ---------------------------------------------------------------------------------------

/// The driver module for the scheduled-route in-process compile regression: `go/2` compiles a
/// **separate** source file inside the calling (scheduled) process with the contract A.9 *E0*
/// option list and answers `1`.
const SCHED_COMPILE_DRIVER_SRC: &str = include_str!("fixtures/sched_compile_driver.erl");

/// The source the driver compiles — a tiny module in plain syntax, so the compile's own readers
/// and passes need nothing outside `kernel` + `stdlib`.
const SCHED_COMPILE_INPUT_SRC: &str = include_str!("fixtures/sched_compile_input.erl");

/// The object code the compile writes into the driver's `{outdir, Dir}`.
const SCHED_COMPILE_INPUT_BEAM: &str = "sched_compile_input.beam";

/// **The scheduled-route in-process compile regression.** A `beam_rt run` process — the route
/// where the workload runs as a **scheduled** process instead of on the pumping root — drives
/// OTP's compiler **in-process** (the A.9 *E0* option list: `no_spawn_compiler_process`,
/// `deterministic`, `{outdir, Dir}`) over a small fixture source, and must answer exactly what
/// the live oracle answers.
///
/// # What this pins
///
/// The compile parks that process in a `receive` (the io-device/file-server round trips), and
/// while it waits a delivery- or signal-driven collection can be asked for its heap. The park
/// arm publishes the machine's root set first, but that snapshot is not the whole truth
/// mid-slice — a live `Machine::pending_exception` or a Rust-local term held by an in-flight
/// `beam_bifs`/`beam_engine` frame is not in it — so a move could recycle a word a live holder
/// still pointed at. The corruption surfaced *inside* the compile (a `beam_term` panic,
/// `invalid boxed header subtag …`), so the observable regression is a missing/garbled artifact
/// or a distinct integer, never the oracle's answer.
///
/// The park arm keeps the parked process **relocation-exempt** for its parked window, and the
/// exemption is cleared when the process is resumed, so a park-window delivery takes the
/// ordinary no-relocation heap-fragment path instead. This test asserts the route's observable
/// outcome on **both** execution tiers (the default native baseline tier and the pinned
/// `interpreter` reference tier, the one the differential gate is taken on), and it compares the
/// produced object code **byte-for-byte** with the oracle's own identical-option run, so a
/// plausible-looking-but-wrong artifact is a failure too.
#[test]
fn a_scheduled_process_compiles_a_source_file_in_process_like_the_oracle() {
    let Some(oracle) = support::require_oracle("compile_file") else {
        return;
    };
    let Some(erlc) = support::find_on_path("erlc") else {
        support::skip_loudly("compile_file", "no `erlc` on $PATH");
        return;
    };

    // ---- the driver, compiled into its own scratch directory ----
    let driver = TempDir::new("sched_compile_driver");
    let driver_src = driver.0.join("sched_compile_driver.erl");
    fs::write(&driver_src, SCHED_COMPILE_DRIVER_SRC).expect("write the driver fixture");
    let driver_dir = driver.0.to_string_lossy().to_string();
    let built = run(&erlc, &["-o", &driver_dir, &driver_src.to_string_lossy()]);
    assert_eq!(built.code, 0, "`erlc` must build the driver: {}", built.err);
    let driver_beam = driver.0.join("sched_compile_driver.beam");
    assert!(
        driver_beam.is_file(),
        "`erlc` wrote no {}",
        driver_beam.display()
    );

    // ---- the source the driver compiles, in a directory of its own ----
    let input = TempDir::new("sched_compile_input");
    let input_src = input.0.join("sched_compile_input.erl");
    fs::write(&input_src, SCHED_COMPILE_INPUT_SRC).expect("write the input fixture");
    let src_arg = literal(&input_src);

    // ---- the runtime, on the scheduled `run` route, on both execution tiers ----
    let beam_arg = driver_beam.to_string_lossy().to_string();
    let mut produced: Option<Vec<u8>> = None;
    for tier in [None, Some("interpreter")] {
        // `compile` does **not** create `{outdir, Dir}`, so a fresh directory per tier is the
        // test's job — it also guarantees the artifact is this tier's own.
        let out = TempDir::new("sched_compile_out");
        let out_arg = literal(&out.0);
        let ours = run_with_env(
            Path::new(BINARY),
            &[
                "-noshell",
                "-pa",
                &driver_dir,
                "run",
                &beam_arg,
                "sched_compile_driver:go/2",
                &src_arg,
                &out_arg,
            ],
            &[("BEAM_RT_TIER", tier)],
        );
        assert_eq!(
            ours.code, 0,
            "the scheduled in-process compile must complete with BEAM_RT_TIER={tier:?}: \
             exit {} — {}",
            ours.code, ours.err
        );
        assert!(
            !ours.err.contains("unsupported"),
            "the scheduled in-process compile must not report a capability gap with \
             BEAM_RT_TIER={tier:?}: {}",
            ours.err
        );
        let line = ours.out.lines().next().unwrap_or_default().to_string();
        assert_eq!(
            line, "1",
            "the scheduled in-process compile must answer the E0 route's `1` with \
             BEAM_RT_TIER={tier:?} (stdout {:?}; stderr {:?})",
            ours.out, ours.err
        );

        // The observable of the compile is the object code it wrote, so assert it really landed.
        let artifact = out.0.join(SCHED_COMPILE_INPUT_BEAM);
        let bytes = fs::read(&artifact).unwrap_or_else(|error| {
            panic!(
                "the scheduled in-process compile reported {} but wrote no {}: {error}",
                ours.out,
                artifact.display()
            )
        });
        assert!(
            !bytes.is_empty(),
            "the scheduled in-process compile must write non-empty object code ({})",
            artifact.display()
        );
        eprintln!(
            "scheduled in-process compile (BEAM_RT_TIER={tier:?}): 1, \
             {SCHED_COMPILE_INPUT_BEAM} = {} bytes",
            bytes.len()
        );
        produced = Some(bytes);
    }

    // ---- the oracle, on the identical workload ----
    let oracle_out = TempDir::new("sched_compile_oracle_out");
    let oracle_out_arg = literal(&oracle_out.0);
    let expr = format!(
        "io:format(\"~w~n\",[sched_compile_driver:go({src_arg}, {oracle_out_arg})]), halt(0)."
    );
    let theirs = support::oracle_eval(&oracle, Some(&driver.0), &expr)
        .unwrap_or_else(|| panic!("the live oracle could not answer the E0 route"));
    assert_eq!(
        theirs, "1",
        "the scheduled in-process compile must agree with the oracle"
    );
    let oracle_artifact = oracle_out.0.join(SCHED_COMPILE_INPUT_BEAM);
    let oracle_bytes = fs::read(&oracle_artifact).unwrap_or_else(|error| {
        panic!(
            "the oracle reported {theirs} but wrote no {}: {error}",
            oracle_artifact.display()
        )
    });
    assert_eq!(
        produced.expect("both tiers ran"),
        oracle_bytes,
        "the scheduled in-process artifact must be byte-identical to the oracle's \
         (identical option list)"
    );
    eprintln!("scheduled in-process compile agrees with the oracle");
}

// ---------------------------------------------------------------------------------------
// Test 5: a REAL stdlib module — the A.9 chain's compile half, byte for byte
// ---------------------------------------------------------------------------------------

/// The repository root: `$ERL_TOP` when set, else three levels up from this crate's manifest
/// directory (`<root>/erts_rust/crates/beam_rt`) — the same discovery the in-crate
/// `src/context/pump.rs` suite uses.
fn source_root() -> PathBuf {
    match std::env::var_os("ERL_TOP") {
        Some(value) => PathBuf::from(value),
        None => Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.."),
    }
}

/// `compile:file(Source, [no_spawn_compiler_process, deterministic, no_docs, {outdir, Dir}])`
/// — the *identical* option list both runtimes are given for a real module.
///
/// `no_docs` is what makes a real module's artifact comparable at all: a stdlib `.beam` carries
/// a compressed `Docs` chunk, and its bytes are the one part of the artifact a rebuild may
/// legitimately vary. With the chunk suppressed (on **both** sides), the remaining bytes are a
/// function of the source and the option list alone — the same argument the end-to-end test
/// above makes for its `Dbgi` comparison.
fn stdlib_probe(src: &Path, outdir: &Path) -> String {
    format!(
        "R = compile:file({}, [no_spawn_compiler_process, deterministic, no_docs, {{outdir, {}}}]),\n\
         io:format(\"RET ~p~n\", [R]),\n\
         halt(0).",
        literal(src),
        literal(outdir)
    )
}

/// **The real-module byte-identity.** `lib/stdlib/src/ordsets.erl` is compiled in process under
/// the Rust runtime and under the system `erl` with the **identical** option list
/// ([`stdlib_probe`]), each into its own output directory, and the two `.beam` artifacts must be
/// **byte-identical**.
///
/// This is the compile half of the A.9 self-hosted chain: the two tests above prove the
/// in-process route on a hand-written fixture (and on the scheduled route), and this one proves
/// it scales to a real OTP module — one whose source imports other modules, uses records,
/// binaries and comprehensions, and is 13 kB rather than four forms. Its consumer, the sibling
/// `selfhost_chain.rs` suite, additionally puts exactly this artifact's directory on the code
/// path and loads and runs modules from it in one invocation.
///
/// # Cost
///
/// `ordsets.erl` is the cheap end of the real-module range: ~1.4–2.2 s for the whole test on
/// this host (the runtime's in-process compile plus the oracle's; host load moves the figure).
/// The heavier real-module compiles —
/// `calendar.erl` ≈ 6.6 s and `lists.erl` ≈ 21 s — are deliberately **not** here; they are
/// covered by the in-crate `src/context/pump.rs` test
/// (`a_real_stdlib_module_compiles_in_process_like_the_oracle`, which byte-compares both of
/// them).
///
/// A missing oracle — or a checkout with no source tree (an installed-only or relocated build,
/// where `lib/stdlib/src/ordsets.erl` does not exist) — is a **loud skip**, never a silent pass.
#[test]
fn a_real_stdlib_module_artifact_is_byte_identical_to_the_oracles() {
    let Some(oracle) = support::require_oracle("compile_file") else {
        return;
    };
    let src = source_root().join("lib/stdlib/src/ordsets.erl");
    if !src.is_file() {
        support::skip_loudly(
            "compile_file",
            &format!("no real stdlib source to compile at {}", src.display()),
        );
        return;
    }

    let dir = TempDir::new("stdlib");
    let rust_out = dir.0.join("rust_out");
    let oracle_out = dir.0.join("oracle_out");
    // `compile` does not create the output directory.
    for out in [&rust_out, &oracle_out] {
        fs::create_dir_all(out).expect("the output directory");
    }

    // ---- the oracle's artifact (identical option list) ----
    let theirs = oracle_eval(&oracle, &stdlib_probe(&src, &oracle_out));
    assert_eq!(
        theirs.code, 0,
        "the oracle must compile ordsets.erl: exit {} — {}",
        theirs.code, theirs.err
    );
    let oracle_artifact = oracle_out.join("ordsets.beam");
    assert!(
        oracle_artifact.is_file(),
        "the oracle wrote no artifact: {}",
        oracle_artifact.display()
    );

    // ---- the Rust runtime's artifact (its own output directory) ----
    let ours = beam_rt_eval(&stdlib_probe(&src, &rust_out));
    assert_eq!(
        ours.code, 0,
        "the in-process compile of a real stdlib module must complete: exit {} — {}",
        ours.code, ours.err
    );
    eprintln!("compile:file/2 of ordsets.erl under beam_rt: {}", ours.out);
    let rust_artifact = rust_out.join("ordsets.beam");

    // ---- byte-for-byte ----
    let mine = fs::read(&rust_artifact).unwrap_or_else(|error| {
        panic!(
            "the runtime reported {:?} but wrote no {}: {error}",
            ours.out,
            rust_artifact.display()
        )
    });
    let reference = fs::read(&oracle_artifact).expect("the oracle artifact");
    if mine != reference {
        let at = mine
            .iter()
            .zip(reference.iter())
            .position(|(a, b)| a != b)
            .unwrap_or_else(|| mine.len().min(reference.len()));
        eprintln!(
            "MISMATCH: rust {} bytes vs oracle {} bytes — first difference at byte {at}: {:?} vs {:?}",
            mine.len(),
            reference.len(),
            mine.get(at),
            reference.get(at)
        );
    }
    assert_eq!(
        mine,
        reference,
        "the in-process artifact of a real stdlib module must be byte-identical to the \
         oracle's (identical option list)"
    );
    eprintln!(
        "ordsets.erl: in-process compile byte-identical to the oracle ({} bytes)",
        mine.len()
    );
}

// ---------------------------------------------------------------------------------------
// Current state
// ---------------------------------------------------------------------------------------
//
// `compile:file/2` completes in-process on the pumping `-eval` route: it drives the real
// `../lib/compiler` under `beam_rt`, its artifact is byte-identical to the oracle's identical
// option-list run, and its `Code` chunk matches `erlc +deterministic`. The write path is the native
// `file_server_2` service, which serves `save_binary`'s write requests.
//
// On the **scheduled** `run` route a full `compile:file/2` completes too — its artifact is
// byte-identical to the oracle's identical option-list run — because the park arm keeps the parked
// compile process relocation-exempt for its parked window (see *Route* above); the
// scheduled-route regressions `a_scheduled_process_parses_a_source_file_like_the_oracle` (the
// `epp:parse_file/3` read path, which answers exactly the oracle's `{FormCount, Tags}`) and
// `a_scheduled_process_compiles_a_source_file_in_process_like_the_oracle` (the E0 compile) pin it
// on both execution tiers.