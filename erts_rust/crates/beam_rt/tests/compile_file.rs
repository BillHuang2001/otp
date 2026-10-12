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
//! instead stops with the typed `unsupported: opcode: run: process blocked (no message available)`
//! — a scheduled slice's out-of-line nested run cannot pump the scheduler, so a callee that parks
//! in a `receive` folds into the engine's `blocked` gap, and `module_observes_its_call_site` routes
//! the whole `compile` module onto that path. That module-wide routing is what keeps the scheduled
//! route **sound**, not merely conservative: the parked machine's live `pending_exception` reason
//! *is* published as a `Registers::extra_roots` word across the park window
//! (`runner/run.rs::publish_parked_roots`), but a scheduled process is not relocation-exempt and the
//! **running** machine's `x`/stack/FR are published into `Process::regs` only at the engine's own
//! heap-admission points (`src/context/gc.rs::ensure_heap_full`/`alloc_words_rooted`) and at a park —
//! never around `ExecutionContext::call_bif` — so a collection at that admission point
//! (`ensure_heap`/`ensure_heap_full` → `collect_or_grow`) would move the heap under a live mid-slice
//! root set that does not reach a term the in-flight call chain holds outside `x`/stack/FR
//! (`Machine::args_scratch`; a `beam_bifs` fast-path BIF's accumulator across a `ctx.call_fun`
//! boundary). A *delivery*-driven collect of a `Running` receiver is already refused
//! (`Process::has_unpublished_roots`). Narrowing the guard's fun-image half is therefore unsound: it
//! turns both self-host corpus cases into a hard mismatch (`beam_term::ops: invalid boxed header
//! subtag 0x2`, `*** Internal compiler error ***`) rather than the typed `blocked` gap.
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

/// Runs `program args` under the shared bounded runner (`/dev/null` stdin, killed at
/// `support::PROBE_TIMEOUT`), failing loudly when the probe could not be run at all.
fn run(program: &Path, args: &[&str]) -> Run {
    let output: Output = support::run_bounded(program, args).unwrap_or_else(|| {
        panic!("`{}` {args:?} could not be run to completion", program.display())
    });
    Run {
        code: output.status.code().unwrap_or(-1),
        out: String::from_utf8_lossy(&output.stdout).trim().to_string(),
        err: String::from_utf8_lossy(&output.stderr).trim().to_string(),
    }
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
// Current state
// ---------------------------------------------------------------------------------------
//
// `compile:file/2` completes in-process on the pumping `-eval` route: it drives the real
// `../lib/compiler` under `beam_rt`, its artifact is byte-identical to the oracle's identical
// option-list run, and its `Code` chunk matches `erlc +deterministic`. The write path is the native
// `file_server_2` service, which serves `save_binary`'s write requests.
//
// On the **scheduled** `run` route a full `compile:file/2` still stops with the typed
// `unsupported: opcode: run: process blocked (no message available)` gap (the module-wide
// `module_observes_its_call_site` guard, see *Route* above), but the `epp:parse_file/3` read path
// it is built on now completes and answers exactly the oracle's `{FormCount, Tags}` — the
// scheduled-route regression `a_scheduled_process_parses_a_source_file_like_the_oracle` pins.