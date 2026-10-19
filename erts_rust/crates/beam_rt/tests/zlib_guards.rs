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

//! The **guard-class / controlling-process** suite for the `zlib` stream API, driven through
//! the real preloaded `zlib.beam` and pinned against the live system `erl` oracle.
//!
//! `zlib`'s 19 non-exported NIF stubs are served by this runtime, so the whole *wrapper* runs as
//! compiled (preloaded) Erlang code and every answer below is a product of the wrapper **and**
//! the NIF behind it. This suite pins the **refusals**: the `error`-class raises the wrapper's
//! own exported surface can reach (`data_error`, `stream_error`, `not_initialized`,
//! `already_initialized`, `badarg`, `not_on_controlling_process`), the `ok`-valued boundaries
//! around them, and the controlling-process transfer both ways.
//!
//! Every observation is normalized to `{value, V}` / `{exception, Class, Reason}` by
//! `zlib_guards:attempt/1`, never to a raw `{'EXIT', {Reason, Stacktrace}}`: a stack-shape
//! difference must not be able to mask a class. The class **is** the assertion.
//!
//! # What this suite pins, and where
//!
//! | # | Guard | Probe (transcript label) | Test |
//! |---|---|---|---|
//! | 1 | `deflateEnd/1` after a non-`finish` `deflate/3` ⇒ `data_error` | `deflate_{none,sync,full}_then_deflateEnd` | [`the_guard_classes_agree_on_every_route_and_tier`] |
//! | 2 | `deflateEnd/1` with no `deflate/3` at all ⇒ `ok` | `fresh_deflateEnd` | same |
//! | 3 | `deflate/3` after a `finish` ⇒ `stream_error` (a second **empty** `finish` is `[]`, the oracle's own shape) | `deflate_{none,sync,data_finish}_after_finish`, `deflate_empty_finish_twice` | same + [`the_guard_classes_agree_on_the_self_contained_eval_route`] |
//! | 4 | a deflate call on an **inflate** stream ⇒ `not_initialized`, and every mirror | `deflate_on_an_inflate_stream`, `inflate_on_a_deflate_stream`, … | same |
//! | 5 | a **foreign** process calling into another's stream ⇒ `not_on_controlling_process` | `foreign_process_*` | [`the_controlling_process_guards_agree_on_every_route_and_tier`] |
//! | 6 | `deflateEnd/1` then `close/1` ⇒ `ok` | `deflateEnd_then_close` | [`the_guard_classes_agree_on_every_route_and_tier`] |
//! | 7 | `close/1` twice ⇒ `not_initialized`; `close/1` on a live stream ⇒ `ok`; on a closed one ⇒ `not_initialized` | `close_twice*`, `close_a_fresh_stream`, `deflate_on_a_closed_stream` | same |
//! | 8 | `set_controlling_process/2` transfers ownership both ways, a dead pid is accepted, a non-pid is `badarg` | `transfer_*`, `set_controlling_process_*` | [`the_controlling_process_guards_agree_on_every_route_and_tier`], [`setting_the_controlling_process_on_a_closed_stream_is_a_known_divergence`] |
//! | 9 | `already_initialized` is reachable from the wrapper (a second `deflateInit/1`, `inflateInit/1` after it, `inflateInit/1` twice) | `deflateInit_twice`, `inflateInit_after_deflateInit`, `inflateInit_twice` | [`the_guard_classes_agree_on_every_route_and_tier`] |
//!
//! # The two routes and the two tiers
//!
//! Every transcript is checked on **four** runtime combinations, and the oracle's own answer is
//! recorded beside them:
//!
//! 1. `beam_rt -noshell -eval EXPR` — the tree-walking `-eval` driver. Two shapes are used:
//!    a **self-contained** expression ([`EXPR_CLASSES`], no fixture needed) and the **compiled
//!    fixture** reached through `-pa` (`zlib_guards:classes()` and friends), which exercises
//!    compiled module code under the same driver;
//! 2. `beam_rt -noshell run <fixture.beam> <module>:<function>/0` — the **scheduled** route, the
//!    same compiled fixture (built at test time by the system `erlc` into a scratch directory)
//!    running as a scheduled process;
//! 3. ×2: the **default** tier (the native baseline JIT) and `BEAM_RT_TIER=interpreter` (the
//!    direct-threaded interpreter) — separate execution engines over the same semantics.
//!
//! Each test states the combinations it covers in its own docs; all four must produce the same
//! transcript, so a divergence on either engine is a failure.
//!
//! **The `run` route prints a result line.** The CLI's `run` action renders the fixture's return
//! value with `~w~n`, and on this route a binary renders as `<<55,56,…>>` (a pre-existing
//! printer difference unrelated to `zlib`). Every fixture function therefore **prints** its own
//! transcript with `io:format/2` and returns `ok`: the compared transcript is the stdout *minus*
//! that one trailing `ok` line, and no binary is ever printed (the payload probe answers
//! `byte_size`/`=:=` witnesses instead).
//!
//! # Known divergences (pinned, not papered over)
//!
//! Neither of these is a `zlib` semantics difference, and both are pinned on **both** sides so a
//! divergence cannot silently disappear or change shape. Neither can be fixed from this suite
//! (`src/` is out of scope), so the assertions record today's pair of answers verbatim.
//!
//! | What | This runtime | The system `erl` |
//! |---|---|---|
//! | `zlib:set_controlling_process(Z, Pid)` on a **closed** stream | `{exception,error,not_initialized}` | `{value,ok}` |
//! | a `not_initialized` / `already_initialized` / `stream_error` raise caught by `try … catch` in a module that does **not** name the atom | the raise is **not caught**: it escapes, exits **1**, `beam_rt: error not_initialized` (empty stdout) | `{exception,error,not_initialized}` |
//!
//! The first is the documented `Requires::Closable` deviation in `beam_bifs/src/zlib/CONTEXT.md`
//! (the C's `set_controller_nif` — `zlib_nif.c:583-602` — imposes **no** state precondition, so
//! it answers `ok` even on a closed stream). The suite pins that the divergence is *exactly* the
//! closed state: the ended and fresh states answer `ok` on both sides.
//!
//! The second is a **runtime** defect, not a `zlib` one: a guard atom that is absent from the
//! raising module's atom table is not resolvable by the catch machinery, so the raise becomes
//! fatal instead of catchable. `tests/fixtures/zlib_guards.erl` therefore **names** the six guard
//! atoms (`zlib_guards:guard_atoms/0`) — that is what makes its transcripts comparable — and
//! `tests/fixtures/zlib_guards_unnamed.erl` is the same probe shape *without* the reference, which
//! [`a_guard_atom_absent_from_the_module_atom_table_escapes_try_catch`] pins. The defect is
//! reported to the workspace root; when it is fixed, that test's two-sided assertions are what
//! trips and tells the next agent to expect oracle parity there.
//!
//! # Toolchain
//!
//! The oracle (`erl`) is resolved from the **filesystem** ([`support::require_oracle`], never by
//! running the tool). Every probe boots this runtime, and boot resolves the *system code path*, so
//! the **installed OTP** is part of the fixture of the suite as well; without it the suite
//! declines with the shared loud-skip banner instead of failing. The `run` route additionally
//! needs the system `erlc`; a missing one is likewise a loud skip ([`build_fixture`]). Nothing
//! here is a silent pass and nothing is a panic for a missing toolchain.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::OnceLock;

// The shared helper module is compiled into every integration-test binary, so this binary would
// otherwise warn on the entry points it does not call.
#[allow(dead_code)]
mod support;

// ---------------------------------------------------------------------------------------
// Names, paths and the tier axis
// ---------------------------------------------------------------------------------------

/// The suite's name in the loud-skip banner.
const SUITE: &str = "zlib_guards";

/// The named fixture module: the probes **with** the guard atoms in its atom table.
const MODULE: &str = "zlib_guards";

/// The unnamed fixture module: the same probe shape **without** the guard atoms
/// ([`a_guard_atom_absent_from_the_module_atom_table_escapes_try_catch`]).
const UNNAMED_MODULE: &str = "zlib_guards_unnamed";

/// The **real** executable under test (the test target's build guarantees it is current).
const BINARY: &str = env!("CARGO_BIN_EXE_beam_rt");

/// The execution tier a probe pins (`beam_rt`'s `PERF.md` tier switch).
#[derive(Clone, Copy)]
enum Tier {
    /// The default: the native baseline JIT for every process entry.
    Native,
    /// The direct-threaded interpreter (`BEAM_RT_TIER=interpreter`), the only kill switch.
    Interpreter,
}

impl Tier {
    /// How a failure names the tier.
    fn label(self) -> &'static str {
        match self {
            Tier::Native => "the default (native baseline) tier",
            Tier::Interpreter => "the interpreter tier (BEAM_RT_TIER=interpreter)",
        }
    }

    /// The `BEAM_RT_TIER` setting: `None` **removes** the variable, so the default tier is
    /// really the default even when the ambient environment sets it.
    fn env_setting(self) -> Option<&'static str> {
        match self {
            Tier::Native => None,
            Tier::Interpreter => Some("interpreter"),
        }
    }
}

/// Both tiers, in the order the probes run.
const TIERS: [Tier; 2] = [Tier::Native, Tier::Interpreter];

// ---------------------------------------------------------------------------------------
// The oracle and the compiler (filesystem discovery only)
// ---------------------------------------------------------------------------------------

/// The live `erl` oracle, resolved **once** (so a missing toolchain is a single loud skip
/// rather than one banner per comparison).
fn oracle() -> Option<PathBuf> {
    static ORACLE: OnceLock<Option<PathBuf>> = OnceLock::new();
    ORACLE
        .get_or_init(|| support::require_oracle(SUITE))
        .clone()
}

/// The oracle, or a loud skip and a `None` the test returns on.
fn oracle_or_skip() -> Option<PathBuf> {
    oracle()
}

/// The compiler beside the resolved oracle first (the same installation), then `$PATH`.
///
/// Filesystem discovery only — a probe must never *run* `erlc` to find out whether it exists
/// (`tests/support/mod.rs`).
fn resolve_erlc() -> Option<PathBuf> {
    if let Some(dir) = oracle().as_deref().and_then(Path::parent) {
        let candidate = dir.join("erlc");
        if support::is_executable(&candidate) {
            return Some(candidate);
        }
    }
    support::find_on_path("erlc")
}

// ---------------------------------------------------------------------------------------
// Bounded probes: this runtime, and the oracle
// ---------------------------------------------------------------------------------------

/// One finished CLI invocation: its exit code and captured output.
struct Cli {
    /// The process exit code (`-1` when killed by a signal, which no probe here does).
    code: i32,
    /// Trimmed stdout.
    out: String,
    /// Trimmed stderr.
    err: String,
}

/// The environment every probe gets: no crash dump litter, and the requested execution tier.
fn probe_env(tier: Tier) -> [(&'static str, Option<&'static str>); 2] {
    [
        ("ERL_CRASH_DUMP_SECONDS", Some("0")),
        ("BEAM_RT_TIER", tier.env_setting()),
    ]
}

/// Runs `program` with `args`, bounded ([`support::run_bounded_with_env`], a 30 s probe bound),
/// so a probe that blocks cannot hang the suite.
fn run(program: &Path, args: &[&str], tier: Tier, what: &str) -> Cli {
    let output =
        support::run_bounded_with_env(program, args, &probe_env(tier)).unwrap_or_else(|| {
            panic!(
                "{what}: `{}` did not finish within the probe bound",
                program.display()
            )
        });
    Cli {
        code: output.status.code().unwrap_or(-1),
        out: String::from_utf8_lossy(&output.stdout).trim().to_string(),
        err: String::from_utf8_lossy(&output.stderr).trim().to_string(),
    }
}

/// This runtime's `-eval` transcript for `expr`, optionally with a scratch directory on `-pa`,
/// which must succeed with a clean stderr.
fn ours_eval(code_dir: Option<&str>, expr: &str, tier: Tier) -> String {
    let what = format!("the `-eval` route on {}", tier.label());
    let mut args: Vec<&str> = vec!["-noshell"];
    if let Some(dir) = code_dir {
        args.push("-pa");
        args.push(dir);
    }
    args.push("-eval");
    args.push(expr);
    let cli = run(Path::new(BINARY), &args, tier, &what);
    assert_eq!(cli.code, 0, "{what} must succeed; stderr:\n{}", cli.err);
    assert!(
        cli.err.is_empty(),
        "{what} must not write to stderr:\n{}",
        cli.err
    );
    cli.out
}

/// This runtime's scheduled-route transcript for `<module>:function/0`.
///
/// The `run` action appends `~w~n` of the fixture's return value, which every fixture function
/// returns as `ok`; the compared transcript is the stdout **before** that line.
fn ours_run(fixture: &Fixture, function: &str, tier: Tier) -> String {
    let what = format!("the scheduled `run` route on {}", tier.label());
    let spec = format!("{}:{function}/0", fixture.module);
    let cli = run(
        Path::new(BINARY),
        &["-noshell", "run", fixture.beam_str(), &spec],
        tier,
        &what,
    );
    assert_eq!(
        cli.code, 0,
        "{what} (`{spec}`) must succeed; stderr:\n{}",
        cli.err
    );
    assert!(
        cli.err.is_empty(),
        "{what} (`{spec}`) must not write to stderr:\n{}",
        cli.err
    );
    let (transcript, result) = cli.out.rsplit_once('\n').unwrap_or_else(|| {
        panic!(
            "{what}: expected a transcript and a result line, got:\n{}",
            cli.out
        )
    });
    assert_eq!(
        result, "ok",
        "{what}: the fixture returns `ok`, so the `run` action's `~w~n` result line is `ok`"
    );
    transcript.to_string()
}

/// The oracle's answer for `expr` — the **same** expression this runtime is given (it ends in
/// `halt(0).`, which the oracle accepts too), with `-s init stop` as the belt-and-braces exit.
fn theirs_eval(oracle: &Path, expr: &str, what: &str) -> String {
    let cli = run(
        oracle,
        &["-noshell", "-eval", expr, "-s", "init", "stop"],
        Tier::Native,
        what,
    );
    assert_eq!(
        cli.code, 0,
        "{what}: the oracle exited with {}; stderr:\n{}",
        cli.code, cli.err
    );
    cli.out
}

/// The oracle running the **compiled fixture** — the same module the scheduled route runs, so
/// the oracle's answer is the one the fixture's own shape produces (not only the `-eval` shape).
///
/// `-pa` is the fixture's **private** scratch directory, which holds only that fixture's `.beam`;
/// `tests/fixtures/` itself must never be handed to the oracle as a single `-pa` (it holds a
/// `lists.beam` that shadows stdlib).
fn theirs_fixture(oracle: &Path, fixture: &Fixture, function: &str) -> String {
    let what = format!("the oracle running `{}:{function}/0`", fixture.module);
    let expr = format!("{}:{function}().", fixture.module);
    let cli = run(
        oracle,
        &[
            "-noshell",
            "-pa",
            fixture.dir_str(),
            "-eval",
            &expr,
            "-s",
            "init",
            "stop",
        ],
        Tier::Native,
        &what,
    );
    assert_eq!(
        cli.code, 0,
        "{what}: the oracle exited with {}; stderr:\n{}",
        cli.code, cli.err
    );
    cli.out
}

// ---------------------------------------------------------------------------------------
// The compiled fixture (route 2 — the scheduled route)
// ---------------------------------------------------------------------------------------

/// A compiled fixture in its own scratch directory (removed on drop).
struct Fixture {
    /// The fixture's module name.
    module: &'static str,
    /// The scratch directory holding this module's `.beam` — and nothing else.
    dir: PathBuf,
    /// The compiled `.beam`.
    beam: PathBuf,
}

impl Fixture {
    /// The compiled image as a `str`, for the CLI's argv.
    fn beam_str(&self) -> &str {
        self.beam
            .to_str()
            .unwrap_or_else(|| panic!("the fixture path is not UTF-8: {}", self.beam.display()))
    }

    /// The scratch directory as a `str`, for `-pa`.
    fn dir_str(&self) -> &str {
        self.dir
            .to_str()
            .unwrap_or_else(|| panic!("the fixture dir is not UTF-8: {}", self.dir.display()))
    }
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

/// Compiles `tests/fixtures/<module>.erl` into a fresh scratch directory, or `None` after a
/// **loud** skip naming what is missing.
///
/// The directory is unique per call (label + module + pid + a process-wide counter) and holds
/// **only** that module's `.beam`, so it is safe to hand the oracle as a single `-pa`. Keeping
/// the two fixtures in *separate* directories is load-bearing for
/// [`a_guard_atom_absent_from_the_module_atom_table_escapes_try_catch`]: the named fixture's
/// atom image must not be loaded beside the unnamed one, or the shared atom table would carry
/// the guard atoms and mask the very defect that test pins.
fn build_fixture(label: &str, module: &'static str) -> Option<Fixture> {
    let Some(erlc) = resolve_erlc() else {
        support::skip_loudly(SUITE, "no `erlc` available (and none beside $ERTS_ORACLE)");
        return None;
    };
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let source = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(format!("{module}.erl"));
    assert!(
        source.is_file(),
        "the committed fixture source is missing: {}",
        source.display()
    );
    let dir = std::env::temp_dir().join(format!(
        "beam_rt_zlib_guards_{label}_{module}_{}_{n}",
        std::process::id()
    ));
    if let Err(error) = std::fs::create_dir_all(&dir) {
        support::skip_loudly(SUITE, &format!("cannot prepare {}: {error}", dir.display()));
        return None;
    }
    let output = match std::process::Command::new(&erlc)
        .arg("-o")
        .arg(&dir)
        .arg(&source)
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
                "`erlc` exited with {:?}: {}",
                output.status.code(),
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        );
        return None;
    }
    let beam = dir.join(format!("{module}.beam"));
    assert!(
        beam.is_file(),
        "`erlc` produced no {}; stderr:\n{}",
        beam.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    Some(Fixture { module, dir, beam })
}

// ---------------------------------------------------------------------------------------
// The goldens
// ---------------------------------------------------------------------------------------
//
// Every transcript below was recorded from the live system `erl` (OTP 29.1.1 / erts-17.1) and is
// re-confirmed against the oracle on every run — the constants are the *record*, the probes are
// the *proof*.

/// `zlib_guards:classes/0` — the guard classes on a single stream (items 1–4, 6, 7, 9).
const GOLDEN_CLASSES: &str = "\
deflate_none_then_deflateEnd {exception,error,data_error}
deflate_sync_then_deflateEnd {exception,error,data_error}
deflate_full_then_deflateEnd {exception,error,data_error}
deflate_finish_then_deflateEnd {value,ok}
fresh_deflateEnd {value,ok}
deflateEnd_twice {exception,error,not_initialized}
deflate_empty_finish_twice {value,[]}
deflate_none_after_finish {exception,error,stream_error}
deflate_sync_after_finish {exception,error,stream_error}
deflate_data_finish_after_finish {exception,error,stream_error}
deflate_on_an_inflate_stream {exception,error,not_initialized}
inflate_on_a_deflate_stream {exception,error,not_initialized}
deflateEnd_on_an_inflate_stream {exception,error,not_initialized}
inflateEnd_on_a_deflate_stream {exception,error,not_initialized}
deflateReset_on_an_inflate_stream {exception,error,not_initialized}
inflateReset_on_a_deflate_stream {exception,error,not_initialized}
deflateParams_on_an_inflate_stream {exception,error,not_initialized}
deflateSetDictionary_on_an_inflate_stream {exception,error,not_initialized}
inflateSetDictionary_on_a_deflate_stream {exception,error,not_initialized}
inflateGetDictionary_on_a_deflate_stream {exception,error,not_initialized}
deflateEnd_then_close {value,ok}
close_a_fresh_stream {value,ok}
close_twice {exception,error,not_initialized}
close_twice_after_inflateInit {exception,error,not_initialized}
deflate_on_a_closed_stream {exception,error,not_initialized}
deflateInit_twice {exception,error,already_initialized}
inflateInit_after_deflateInit {exception,error,already_initialized}
inflateInit_twice {exception,error,already_initialized}
close_a_non_handle {exception,error,badarg}
deflateInit_a_non_handle {exception,error,badarg}
deflate_a_non_handle {exception,error,badarg}
inflate_a_non_handle {exception,error,badarg}";

/// `zlib_guards:controlling/0` — `not_on_controlling_process` and the ownership transfer
/// (items 5 and 8).
const GOLDEN_CONTROLLING: &str = "\
foreign_process_deflate {value,{exception,error,not_on_controlling_process}}
foreign_process_close {value,{{exception,error,not_on_controlling_process},ok}}
foreign_process_deflateInit {value,{exception,error,not_on_controlling_process}}
foreign_process_deflateEnd {value,{exception,error,not_on_controlling_process}}
foreign_process_inflate_on_deflate_stream {value,{exception,error,not_on_controlling_process}}
foreign_process_on_a_fresh_stream {value,{exception,error,not_on_controlling_process}}
foreign_process_close_on_a_closed_stream {value,{exception,error,not_on_controlling_process}}
transfer_new_owner_drives {value,{{exception,error,not_on_controlling_process},{value,true}}}
transfer_back_round_trip {value,{36,true,false}}
transfer_to_a_dead_pid {value,{{value,ok},{exception,error,not_on_controlling_process}}}
transfer_to_a_non_pid {value,{{exception,error,badarg},ok}}
transfer_to_self {value,true}";

/// `zlib_guards:closed_controller/0` on **this** runtime — the divergence is line 1 only (see
/// the module docs): `set_controller_nif` has no state precondition in the C, so the oracle
/// answers `ok` there.
const GOLDEN_CLOSED_CONTROLLER_OURS: &str = "\
set_controlling_process_on_a_closed_stream {exception,error,not_initialized}
set_controlling_process_on_an_ended_stream {value,ok}
set_controlling_process_on_a_fresh_stream {value,ok}";

/// The **oracle's** `zlib_guards:closed_controller/0` — identical to
/// [`GOLDEN_CLOSED_CONTROLLER_OURS`] except line 1.
const GOLDEN_CLOSED_CONTROLLER_ORACLE: &str = "\
set_controlling_process_on_a_closed_stream {value,ok}
set_controlling_process_on_an_ended_stream {value,ok}
set_controlling_process_on_a_fresh_stream {value,ok}";

/// [`EXPR_CLASSES`]'s transcript — the same classes as [`GOLDEN_CLASSES`], reached through a
/// self-contained `-eval` expression instead of the compiled fixture, so the two drivers are
/// compared on the classes they can both express. (The expression has no stream handoff, so its
/// `foreign_process_close` line is the bare class rather than the fixture's
/// `{value,{Class,ok}}` witness.)
const GOLDEN_WALKER: &str = "\
deflate_none_then_deflateEnd {exception,error,data_error}
deflateEnd_twice {exception,error,not_initialized}
deflateInit_twice {exception,error,already_initialized}
deflate_none_after_finish {exception,error,stream_error}
close_a_non_handle {exception,error,badarg}
close_twice {exception,error,not_initialized}
deflate_on_an_inflate_stream {exception,error,not_initialized}
foreign_process_close {exception,error,not_on_controlling_process}";

/// The self-contained `-eval` expression: one probe of every guard family, printed in the same
/// `label {observation}` shape the fixture uses.
///
/// `A/1` is the normalized observation (`zlib_guards:attempt/1`'s twin) and `P/2` prints it with
/// `~w` (never `~p`, which wraps at ~80 columns and would make the transcript
/// printer-dependent). The last probe runs the call in a **second process**, so
/// `not_on_controlling_process` is pinned on this driver too.
const EXPR_CLASSES: &str = concat!(
    "A = fun(F) -> try {value,F()} catch C:R -> {exception,C,R} end end, ",
    "P = fun(L,R) -> io:format(\"~s ~w~n\",[L,R]) end, ",
    // 1. the documented data_error: a deflateEnd after a non-finish deflate
    "Z1 = zlib:open(), ok = zlib:deflateInit(Z1), _ = zlib:deflate(Z1,<<\"h\">>,none), ",
    "P(\"deflate_none_then_deflateEnd\", A(fun() -> zlib:deflateEnd(Z1) end)), ",
    // 2. not_initialized: deflateEnd twice
    "Z2 = zlib:open(), ok = zlib:deflateInit(Z2), _ = zlib:deflate(Z2,<<\"h\">>,finish), ",
    "ok = zlib:deflateEnd(Z2), ",
    "P(\"deflateEnd_twice\", A(fun() -> zlib:deflateEnd(Z2) end)), ",
    // 3. already_initialized: a second init on the same handle
    "Z3 = zlib:open(), ok = zlib:deflateInit(Z3), ",
    "P(\"deflateInit_twice\", A(fun() -> zlib:deflateInit(Z3) end)), ",
    // 4. stream_error: any further input after the first finish
    "Z4 = zlib:open(), ok = zlib:deflateInit(Z4), _ = zlib:deflate(Z4,<<\"h\">>,finish), ",
    "P(\"deflate_none_after_finish\", A(fun() -> zlib:deflate(Z4,<<\"m\">>,none) end)), ",
    // 5. badarg: a term that is not a stream handle
    "P(\"close_a_non_handle\", A(fun() -> zlib:close(42) end)), ",
    // 6. close twice
    "Z5 = zlib:open(), ok = zlib:close(Z5), ",
    "P(\"close_twice\", A(fun() -> zlib:close(Z5) end)), ",
    // 7. the wrong-kind mirror
    "Z6 = zlib:open(), ok = zlib:inflateInit(Z6), ",
    "P(\"deflate_on_an_inflate_stream\", A(fun() -> zlib:deflate(Z6,<<\"h\">>,none) end)), ",
    // 8. not_on_controlling_process, from a second process
    "Self = self(), Z7 = zlib:open(), ok = zlib:deflateInit(Z7), ",
    "_ = spawn(fun() -> Self ! {c, A(fun() -> zlib:close(Z7) end)} end), ",
    "R7 = receive {c,R} -> R after 5000 -> timeout end, ",
    "P(\"foreign_process_close\", R7), ",
    "halt(0)."
);

/// The unnamed fixture's probes: `(function, what the class is, the oracle's transcript)`.
///
/// The oracle's line is the whole transcript the unnamed fixture prints under the system `erl`;
/// this runtime never gets that far (the raise escapes — see the module docs).
const UNNAMED_PROBES: [(&str, &str, &str); 3] = [
    (
        "deflate_end_twice",
        "not_initialized",
        "deflateEnd_twice {exception,error,not_initialized}",
    ),
    (
        "deflate_init_twice",
        "already_initialized",
        "deflateInit_twice {exception,error,already_initialized}",
    ),
    (
        "after_finish",
        "stream_error",
        "deflate_none_after_finish {exception,error,stream_error}",
    ),
];

// ---------------------------------------------------------------------------------------
// 1. The guard classes, on the self-contained `-eval` driver
// ---------------------------------------------------------------------------------------

/// Covers the **self-contained `-eval`** combinations (both tiers) plus the oracle on the same
/// expression: the guard families of items 1, 3, 4, 7, 9 and the `not_on_controlling_process`
/// half of item 5.
///
/// This is the driver that reaches `zlib` through the runtime's own `-eval` walker rather than
/// through a compiled fixture, so it is the one route where a guard class is pinned without any
/// dependence on a module's atom table (see the module docs' second divergence).
#[test]
fn the_guard_classes_agree_on_the_self_contained_eval_route() {
    let Some(oracle) = oracle_or_skip() else {
        return;
    };
    for tier in TIERS {
        let ours = ours_eval(None, EXPR_CLASSES, tier);
        assert_eq!(
            ours,
            GOLDEN_WALKER,
            "the self-contained `-eval` transcript on {}",
            tier.label()
        );
    }
    let theirs = theirs_eval(
        &oracle,
        EXPR_CLASSES,
        "the oracle on the same `-eval` expression",
    );
    assert_eq!(
        theirs, GOLDEN_WALKER,
        "the oracle's `-eval` transcript must be the same recording"
    );
}

// ---------------------------------------------------------------------------------------
// 2. The guard classes, on the compiled fixture (every route and tier)
// ---------------------------------------------------------------------------------------

/// Covers **all four** combinations of the compiled fixture (`-eval`-with-`-pa` and scheduled
/// `run`, each on the default and the interpreter tier), plus the oracle running the same
/// compiled fixture: items 1, 2, 3, 4, 6, 7 and 9 of the module docs' table.
#[test]
fn the_guard_classes_agree_on_every_route_and_tier() {
    let Some(oracle) = oracle_or_skip() else {
        return;
    };
    let Some(fixture) = build_fixture("classes", MODULE) else {
        return;
    };
    for tier in TIERS {
        let eval = ours_eval(
            Some(fixture.dir_str()),
            &format!("{MODULE}:classes(),halt(0)."),
            tier,
        );
        assert_eq!(
            eval,
            GOLDEN_CLASSES,
            "`{MODULE}:classes/0` through the `-eval` driver on {}",
            tier.label()
        );
        let scheduled = ours_run(&fixture, "classes", tier);
        assert_eq!(
            scheduled,
            GOLDEN_CLASSES,
            "`{MODULE}:classes/0` through the scheduled `run` route on {}",
            tier.label()
        );
    }
    let theirs = theirs_fixture(&oracle, &fixture, "classes");
    assert_eq!(
        theirs, GOLDEN_CLASSES,
        "the oracle running the compiled `{MODULE}:classes/0` must be the same recording"
    );
}

// ---------------------------------------------------------------------------------------
// 3. The controlling process (items 5 and 8)
// ---------------------------------------------------------------------------------------

/// Covers **all four** combinations of the compiled fixture plus the oracle running it: a
/// foreign process refused on every stream state, the ownership transfer both ways, the
/// accepted-but-dead pid and the `badarg` non-pid.
///
/// The refusal is raised by the *first* NIF the wrapper reaches (`getStash_nif/1` —
/// `restore_progress/2`, ahead of every `deflate`/`inflate`), which is why it wins over the
/// stream's own state: `foreign_process_close_on_a_closed_stream` and
/// `foreign_process_on_a_fresh_stream` both answer `not_on_controlling_process`.
#[test]
fn the_controlling_process_guards_agree_on_every_route_and_tier() {
    let Some(oracle) = oracle_or_skip() else {
        return;
    };
    let Some(fixture) = build_fixture("controlling", MODULE) else {
        return;
    };
    for tier in TIERS {
        let eval = ours_eval(
            Some(fixture.dir_str()),
            &format!("{MODULE}:controlling(),halt(0)."),
            tier,
        );
        assert_eq!(
            eval,
            GOLDEN_CONTROLLING,
            "`{MODULE}:controlling/0` through the `-eval` driver on {}",
            tier.label()
        );
        let scheduled = ours_run(&fixture, "controlling", tier);
        assert_eq!(
            scheduled,
            GOLDEN_CONTROLLING,
            "`{MODULE}:controlling/0` through the scheduled `run` route on {}",
            tier.label()
        );
    }
    let theirs = theirs_fixture(&oracle, &fixture, "controlling");
    assert_eq!(
        theirs, GOLDEN_CONTROLLING,
        "the oracle running the compiled `{MODULE}:controlling/0` must be the same recording"
    );
}

// ---------------------------------------------------------------------------------------
// 4. The one divergence: set_controlling_process/2 on a CLOSED stream
// ---------------------------------------------------------------------------------------

/// Pins the **known divergence** (module docs, table 1) on all four combinations plus the
/// oracle: `zlib:set_controlling_process/2` answers `ok` on a closed stream under the system
/// `erl` — the C's `set_controller_nif` (`zlib_nif.c:583-602`) has no state precondition — while
/// this runtime raises `not_initialized`, because `beam_bifs`'s `Requires::Closable` refuses a
/// `Closed` stream.
///
/// Both sides are asserted **verbatim**, so the divergence cannot change shape unnoticed and a
/// fix is caught: the day this runtime answers `ok`, this test fails and
/// [`GOLDEN_CLOSED_CONTROLLER_OURS`] must become [`GOLDEN_CLOSED_CONTROLLER_ORACLE`].
///
/// The other two lines pin the boundary of the divergence: the **ended** and **fresh** states
/// already agree with the oracle.
#[test]
fn setting_the_controlling_process_on_a_closed_stream_is_a_known_divergence() {
    let Some(oracle) = oracle_or_skip() else {
        return;
    };
    let Some(fixture) = build_fixture("closed_controller", MODULE) else {
        return;
    };
    for tier in TIERS {
        let eval = ours_eval(
            Some(fixture.dir_str()),
            &format!("{MODULE}:closed_controller(),halt(0)."),
            tier,
        );
        assert_eq!(
            eval,
            GOLDEN_CLOSED_CONTROLLER_OURS,
            "`{MODULE}:closed_controller/0` through the `-eval` driver on {}",
            tier.label()
        );
        let scheduled = ours_run(&fixture, "closed_controller", tier);
        assert_eq!(
            scheduled,
            GOLDEN_CLOSED_CONTROLLER_OURS,
            "`{MODULE}:closed_controller/0` through the scheduled `run` route on {}",
            tier.label()
        );
    }
    let theirs = theirs_fixture(&oracle, &fixture, "closed_controller");
    assert_eq!(
        theirs, GOLDEN_CLOSED_CONTROLLER_ORACLE,
        "the oracle's `set_controlling_process/2` on a closed stream is `ok` — the C imposes no \
         state precondition, which is exactly the divergence this test pins"
    );

    // The divergence is line 1 and nothing else: both sides agree on the remaining lines.
    let ours_lines: Vec<&str> = GOLDEN_CLOSED_CONTROLLER_OURS.lines().skip(1).collect();
    let oracle_lines: Vec<&str> = GOLDEN_CLOSED_CONTROLLER_ORACLE.lines().skip(1).collect();
    assert_eq!(
        ours_lines, oracle_lines,
        "the ended/fresh `set_controlling_process/2` answers must agree with the oracle"
    );
}

// ---------------------------------------------------------------------------------------
// 5. The second divergence: a guard atom the module's atom table does not carry
// ---------------------------------------------------------------------------------------

/// Pins the **runtime** defect the module docs describe (divergence table 2), on both routes and
/// both tiers, for all three affected classes.
///
/// `tests/fixtures/zlib_guards_unnamed.erl` is the same probe shape as the named fixture **minus**
/// the atom reference, so `not_initialized` / `already_initialized` / `stream_error` never enter
/// its atom table. On this runtime the raise is then not catchable by that module's own
/// `try … catch`: it escapes `caught/1`, nothing is printed, and the process dies with
/// `beam_rt: error <atom>` (exit 1). The system `erl` catches it and prints the class.
///
/// The **positive control** is the same class in the named fixture, which is caught — so this is
/// an atom-table (name-resolution) dependence, not a difference in the raised reason. Both sides
/// are asserted verbatim; when the runtime defect is fixed this test fails and the assertions
/// become the oracle's lines (that is the signal to invert them).
///
/// Each fixture is compiled into its **own** scratch directory: loading the named fixture beside
/// the unnamed one would put the guard atoms in the shared atom table and mask the defect.
#[test]
fn a_guard_atom_absent_from_the_module_atom_table_escapes_try_catch() {
    let Some(oracle) = oracle_or_skip() else {
        return;
    };
    let Some(unnamed) = build_fixture("unnamed", UNNAMED_MODULE) else {
        return;
    };
    let Some(named) = build_fixture("named_control", MODULE) else {
        return;
    };
    // The positive control's transcript, computed once: the same classes in the module that
    // *names* the guard atoms are caught, so the difference below is the atom table and nothing
    // else.
    let control = ours_eval(
        Some(named.dir_str()),
        &format!("{MODULE}:classes(),halt(0)."),
        Tier::Native,
    );
    for (function, class, oracle_line) in UNNAMED_PROBES {
        // The oracle catches every one of them.
        let theirs = theirs_fixture(&oracle, &unnamed, function);
        assert_eq!(
            theirs, oracle_line,
            "the oracle catches `{class}` in `{UNNAMED_MODULE}:{function}/0`"
        );
        let fatal = format!("beam_rt: error {class}");

        for tier in TIERS {
            // The `-eval` driver over the compiled module.
            let eval = run(
                Path::new(BINARY),
                &[
                    "-noshell",
                    "-pa",
                    unnamed.dir_str(),
                    "-eval",
                    &format!("{UNNAMED_MODULE}:{function}(),halt(0)."),
                ],
                tier,
                &format!("`{UNNAMED_MODULE}:{function}/0` through the `-eval` driver"),
            );
            assert_eq!(
                (eval.code, eval.out.as_str(), eval.err.as_str()),
                (1, "", fatal.as_str()),
                "the `{class}` raise escapes `try … catch` on the `-eval` driver on {} — the \
                 module's atom table does not carry the atom, and the raise is not caught",
                tier.label()
            );

            // The scheduled route over the same module.
            let scheduled = run(
                Path::new(BINARY),
                &[
                    "-noshell",
                    "run",
                    unnamed.beam_str(),
                    &format!("{UNNAMED_MODULE}:{function}/0"),
                ],
                tier,
                &format!("`{UNNAMED_MODULE}:{function}/0` through the scheduled `run` route"),
            );
            assert_eq!(
                (
                    scheduled.code,
                    scheduled.out.as_str(),
                    scheduled.err.as_str()
                ),
                (1, "", fatal.as_str()),
                "the `{class}` raise escapes `try … catch` on the scheduled route on {}",
                tier.label()
            );
        }

        // The positive control: the same class in the module that *names* the atoms is caught.
        assert!(
            control.lines().any(|line| line == oracle_line),
            "the named fixture must catch `{class}` in the same class shape the oracle prints \
             (`{oracle_line}`), so the escape above is an atom-table dependence; transcript:\n\
             {control}"
        );
    }
}

// ---------------------------------------------------------------------------------------
// 6. already_initialized is reachable from the wrapper (item 9)
// ---------------------------------------------------------------------------------------

/// Item 9's explicit answer: `already_initialized` **is** reachable from `zlib`'s exported
/// surface — no case has to be invented — and it is reached by three distinct wrapper paths, all
/// of which the oracle reports identically:
///
/// * `deflateInit/1` twice on the same handle (`deflateInit_nif/6`'s `Requires::Init`);
/// * `inflateInit/1` on a deflate-initialized handle (the same precondition, wrong kind);
/// * `inflateInit/1` twice.
///
/// The three lines live in [`GOLDEN_CLASSES`]; this test pins them at whole-transcript
/// granularity on the same four combinations and re-derives them from the golden so a change of
/// class is reported against the item it belongs to.
#[test]
fn already_initialized_is_reachable_from_the_wrapper() {
    let Some(oracle) = oracle_or_skip() else {
        return;
    };
    let Some(fixture) = build_fixture("already_initialized", MODULE) else {
        return;
    };
    let expected = [
        "deflateInit_twice {exception,error,already_initialized}",
        "inflateInit_after_deflateInit {exception,error,already_initialized}",
        "inflateInit_twice {exception,error,already_initialized}",
    ];
    for line in expected {
        assert!(
            GOLDEN_CLASSES.lines().any(|golden| golden == line),
            "the golden must record `{line}`"
        );
    }
    for tier in TIERS {
        let transcript = ours_run(&fixture, "classes", tier);
        for line in expected {
            assert!(
                transcript.lines().any(|probe| probe == line),
                "`{MODULE}:classes/0` on {} must answer `{line}`",
                tier.label()
            );
        }
    }
    let theirs = theirs_fixture(&oracle, &fixture, "classes");
    for line in expected {
        assert!(
            theirs.lines().any(|probe| probe == line),
            "the oracle must answer `{line}` — `already_initialized` is a reachable wrapper class"
        );
    }
}
