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

//! The **A.7 boot integration suite**: the preloaded-module set, the boot-script executor
//! and the `erlang:pre_loaded/0` / `code:which/1` / `code:all_loaded/0` surface.
//!
//! Contract A.7 (OTP application contracts) requires the runtime to provide the
//! *preloaded* modules — the code that is loaded before any other `.beam` is read — and to
//! execute a real OTP boot script. `src/boot.rs` (the name registry + the atom seed) and
//! `src/boot/bootscript.rs` (the reader/executor) implement it; this suite pins the
//! **observable** facts from the outside, so a regression is caught even where the
//! in-crate unit tests would not look:
//!
//! 1. the whole 22-module preloaded set is really loaded into the code server's cache, and
//!    booting twice is a no-op ([`boot_loads_the_whole_preloaded_set_and_is_idempotent`]);
//! 2. `code:which/1`, `code:is_loaded/1` and `code:all_loaded/0` at the **CLI** boundary —
//!    the preloaded names come first, in `erlang:pre_loaded/0`'s order
//!    ([`code_which_is_loaded_and_all_loaded_put_preloaded_first`]);
//! 3. `erlang:pre_loaded/0` equals the **live oracle's** answer ([`pre_loaded_matches_the_live_oracle`]);
//! 4. the real committed `bootstrap/bin/start.boot` loads its `primLoad` batches and then
//!    stops with an **honest typed error** at the first directive M1 cannot run
//!    ([`the_real_start_boot_loads_modules_then_stops_honestly`]);
//! 5. a synthetic boot script — compiled at test time with the system toolchain — executes
//!    end to end and its `{apply, …}` step's side effect is observable
//!    ([`a_synthetic_boot_script_executes_end_to_end`]);
//! 6. the **CLI** really drives that executor for `-boot FILE` (contract A.7/A.9): a
//!    synthetic script's `{apply, …}` reaches stdout through the executable
//!    ([`the_cli_drives_the_boot_script_of_a_boot_flag`]), the real `start.boot`'s honest
//!    stop is reported as a typed runtime error, never a panic
//!    ([`the_cli_reports_the_boot_scripts_honest_stop_as_a_typed_error`]), and a boot with
//!    **no** `-boot` keeps the preloaded-only drive
//!    ([`a_boot_without_the_boot_flag_keeps_the_preloaded_only_drive`]).
//!
//! Every test that needs the system Erlang install (the erts application's `ebin` holds the
//! preloaded `.beam`s; the oracle is `erl`) skips **loudly** — a clear `eprintln!` and an
//! early `return` — when it is absent. Nothing here silently passes.
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use beam_common::BeamError;
use beam_rt::{Runtime, RuntimeConfig};

/// The committed `start.boot` of this checkout (the oracle-identical real script).
///
/// `CARGO_MANIFEST_DIR` is `<repo>/erts_rust/crates/beam_rt`, so the repository root (which
/// holds `bootstrap/`) is three levels up.
const START_BOOT: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../../bootstrap/bin/start.boot"
);

// ---------------------------------------------------------------------------
// Harnesses
// ---------------------------------------------------------------------------

/// Runs the built `beam_rt` binary with `args`.
fn beam_rt(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_beam_rt"))
        .args(args)
        .output()
        .unwrap_or_else(|error| panic!("spawning beam_rt {args:?} failed: {error}"))
}

/// The stdout of a finished run, as text.
fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// The stderr of a finished run, as text.
fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// One `beam_rt -noshell -eval <expr>` probe, asserting the full success contract: exit `0`,
/// **silence on stderr** and `expected` (which includes its trailing newline) on stdout.
fn eval_probe(expr: &str, expected: &str) {
    let output = beam_rt(&["-noshell", "-eval", expr]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "{expr}: exit (stderr {:?})",
        stderr(&output)
    );
    assert_eq!(stdout(&output), expected, "{expr}: stdout");
    assert!(output.stderr.is_empty(), "{expr}: stderr {:?}", stderr(&output));
}

/// Runs the system `erl` oracle with `program` (which must `halt/0` itself), or `None` —
/// **loudly** — when `erl` is unavailable or fails to run.
fn oracle(program: &str) -> Option<Output> {
    match Command::new("erl").args(["-noshell", "-eval", program]).output() {
        Ok(output) if output.status.success() => Some(output),
        Ok(output) => {
            eprintln!(
                "SKIP: `erl` exited with {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
            None
        }
        Err(error) => {
            eprintln!("SKIP: cannot run `erl`: {error}");
            None
        }
    }
}

/// A runtime whose boot sequence really ran — the preloaded `.beam`s live in the erts
/// application's `ebin`, so this needs a system Erlang install. Returns `None` (and prints a
/// clear `SKIP`) when that install is unavailable; the caller then returns early.
fn booted_runtime() -> Option<Runtime> {
    let mut rt = Runtime::with_defaults().expect("the default runtime");
    match rt.boot() {
        Ok(()) => Some(rt),
        Err(error) => {
            eprintln!("SKIP: boot needs the system Erlang install ({error})");
            None
        }
    }
}

// ---------------------------------------------------------------------------
// 1. Preloaded loading is real
// ---------------------------------------------------------------------------

#[test]
fn boot_loads_the_whole_preloaded_set_and_is_idempotent() {
    let mut rt = Runtime::with_defaults().expect("the default runtime");
    // Nothing is loaded before boot.
    assert!(!rt.is_booted());
    assert!(rt.preloaded_loaded().is_empty(), "nothing is loaded before boot");
    if let Err(error) = rt.boot() {
        eprintln!("SKIP: boot needs the system Erlang install ({error})");
        return;
    }

    // The three modules the boot contract names as the runtime's own foundation.
    for name in ["erlang", "init", "erts_internal"] {
        assert!(rt.is_preloaded_loaded(name), "{name} must be loaded");
    }
    // The whole set, not just the names.
    assert_eq!(rt.preloaded_loaded().len(), 22, "the oracle has 22 preloaded modules");
    let names: Vec<String> = rt.preloaded().names().map(str::to_string).collect();
    assert_eq!(names.len(), 22);
    assert_eq!(
        rt.preloaded_loaded(),
        names,
        "and `preloaded_loaded` is the registry's order, whole"
    );
    for name in rt.preloaded().names() {
        assert!(
            rt.code_server().is_loaded(name),
            "{name}.beam must be in the shared cache after boot"
        );
    }
    // `is_preloaded_loaded` is `false` for a module outside the preloaded registry.
    assert!(!rt.is_preloaded_loaded("lists"));

    // Idempotent: a second boot is a no-op and loads nothing new.
    let before = rt.preloaded_loaded();
    let loaded_before = rt.loaded_modules().len();
    rt.boot().expect("booting twice is a no-op");
    assert!(rt.is_booted());
    assert_eq!(rt.preloaded_loaded(), before, "a second boot must not change the cache");
    assert_eq!(rt.loaded_modules().len(), loaded_before);
}

// ---------------------------------------------------------------------------
// 2. code:which/1 + code:is_loaded/1 + code:all_loaded/0 at the CLI boundary
// ---------------------------------------------------------------------------

#[test]
fn code_which_is_loaded_and_all_loaded_put_preloaded_first() {
    // The CLI probe boots the runtime, so it needs the system install too.
    if booted_runtime().is_none() {
        return;
    }

    // `code:which(erlang)` is the atom `preloaded` for an emulator-image module.
    eval_probe(
        "io:format(\"~p~n\",[code:which(erlang)])",
        "preloaded\n",
    );
    // `code:is_loaded(erlang)` answers `{file, preloaded}`.
    eval_probe(
        "io:format(\"~p~n\",[code:is_loaded(erlang)])",
        "{file,preloaded}\n",
    );
    // `erlang` is in `code:all_loaded/0`, carrying the `preloaded` path.
    eval_probe(
        "io:format(\"~p~n\",[lists:keyfind(erlang, 1, code:all_loaded())])",
        "{erlang,preloaded}\n",
    );
    // The **preloaded names come first** in `code:all_loaded/0`, in `erlang:pre_loaded/0`'s
    // order: the runtime loads them before any other code, so the first 22 entries of
    // `all_loaded` are exactly the preloaded set. (The `lists` module this probe itself
    // loads lands *after* them, so the prefix is stable.)
    eval_probe(
        "io:format(\"~p~n\",[[M || {M,_} <- lists:sublist(code:all_loaded(), 22)] =:= erlang:pre_loaded()])",
        "true\n",
    );
}

// ---------------------------------------------------------------------------
// 3. erlang:pre_loaded/0 == the live oracle
// ---------------------------------------------------------------------------

#[test]
fn pre_loaded_matches_the_live_oracle() {
    if booted_runtime().is_none() {
        return;
    }
    let Some(oracle_output) =
        oracle("io:format(\"~p~n\",[erlang:pre_loaded()]),halt(0).")
    else {
        return;
    };

    // The runtime's own answer, byte-for-byte identical to the oracle's `~p` rendering
    // (which wraps the list at the same width).
    eval_probe(
        "io:format(\"~p~n\",[erlang:pre_loaded()])",
        &stdout(&oracle_output),
    );
    // And the count is the contract's 22.
    eval_probe(
        "io:format(\"~p~n\",[length(erlang:pre_loaded())])",
        "22\n",
    );
}

// ---------------------------------------------------------------------------
// 4. A real boot script: loads modules, then stops honestly
// ---------------------------------------------------------------------------

#[test]
fn the_real_start_boot_loads_modules_then_stops_honestly() {
    let Some(mut rt) = booted_runtime() else {
        return;
    };
    let path = Path::new(START_BOOT);
    if !path.is_file() {
        eprintln!("SKIP: {START_BOOT} is absent");
        return;
    }

    // The real `start.boot` cannot fully boot in Milestone 1: `boot_from_script` walks the
    // directives in order, loading every `primLoad` batch, and stops at the first directive
    // the runtime cannot run with a typed error. It cannot panic or hang — `execute` drives
    // the engine directly (never `run_scheduler`), so a blocking receive is the engine's
    // typed `error::blocked()`. A watchdog thread is impossible here because `Runtime` is
    // `!Send`; the boundedness is the executor's own by construction.
    let error = rt
        .boot_from_script(path)
        .expect_err("the real start.boot cannot fully boot in Milestone 1");
    assert!(
        matches!(error, BeamError::Raise(_)),
        "the stop must be a typed BeamError, never a panic: {error}"
    );

    // The `primLoad` batches really loaded before the stop, and they are **not** preloaded
    // modules — so this is a genuine script-driven load.
    for module in ["lists", "gen_server", "application_starter"] {
        assert!(
            rt.module(module).is_some(),
            "{module} must have been loaded by the script before the stop"
        );
        assert!(
            !rt.is_preloaded_loaded(module),
            "{module} is not a preloaded module, so the load came from the script"
        );
    }
    eprintln!("note: the real start.boot stops with: {error}");
}

// ---------------------------------------------------------------------------
// 5. A synthetic boot script, executed end to end
// ---------------------------------------------------------------------------

/// The fixture module the synthetic boot script applies.
///
/// Compiled at test time with the system `erlc` (never committed): the `{apply, …}` step
/// calls `mark/0`, whose process-dictionary side effect `marked/0` reads back through the
/// runtime's own API, and which also writes a line to stdout — the only one of the two the
/// `beam_rt` **CLI** can observe (§6).
const FIXTURE_ERL: &str = r#"%% Generated by `beam_rt/tests/boot.rs` (never committed).
-module(boot_fixture).
-export([mark/0, marked/0]).
%% The observable side effect the boot script's `apply` produces: a dictionary entry on
%% the process the script runs as (the runtime's own `init` process), plus a line on stdout
%% so the same `apply` is observable from *outside* the runtime.
mark() -> erlang:put(boot_mark, seen), io:format("boot script apply ran~n").
%% Reads that entry back.
marked() -> erlang:get(boot_mark).
"#;

/// The line [`FIXTURE_ERL`]'s `mark/0` prints.
const MARK_LINE: &str = "boot script apply ran";

/// A scratch directory holding a compiled fixture and its `.boot`, removed on drop.
struct Fixture {
    dir: PathBuf,
}

impl Fixture {
    /// Compiles the fixture module and writes the synthetic `.boot` into a fresh scratch
    /// directory, or `None` — **loudly** — when the toolchain is unavailable.
    ///
    /// Both `erlc` (the fixture) and `erl` (the `.boot` image, written exactly as the real
    /// `init` would read it) are required; either being absent is a loud skip.
    fn build(label: &str) -> Option<Fixture> {
        let name = format!("beam_rt_boot_it_{label}_{}", std::process::id());
        let dir = std::env::temp_dir().join(name);
        if let Err(error) = std::fs::create_dir_all(&dir) {
            eprintln!("SKIP boot: cannot create {}: {error}", dir.display());
            return None;
        }
        let source = dir.join("boot_fixture.erl");
        if let Err(error) = std::fs::write(&source, FIXTURE_ERL) {
            eprintln!("SKIP boot: cannot write {}: {error}", source.display());
            return None;
        }
        // Compile the fixture module with `+deterministic` (path-free, like the committed
        // fixtures).
        match Command::new("erlc")
            .arg("+deterministic")
            .arg("-o")
            .arg(&dir)
            .arg(&source)
            .output()
        {
            Ok(output) if output.status.success() => {}
            Ok(output) => {
                eprintln!(
                    "SKIP boot: `erlc` exited with {}: {}",
                    output.status,
                    String::from_utf8_lossy(&output.stderr)
                );
                return None;
            }
            Err(error) => {
                eprintln!("SKIP boot: cannot run `erlc`: {error}");
                return None;
            }
        }
        let beam = dir.join("boot_fixture.beam");
        if !beam.is_file() {
            eprintln!("SKIP boot: `erlc` produced no {}", beam.display());
            return None;
        }

        // The synthetic script: the preloaded set, a code-path entry, one module load and
        // the observed `apply`, in the exact `{script, {Name, Vsn}, Instructions}` shape the
        // real `init` reads.
        let boot = dir.join("synthetic.boot");
        let dir_text = dir.display();
        let program = format!(
            "ok = file:write_file(\"{boot}\", term_to_binary({{script,{{\"t\",\"1\"}},[\
             {{preLoaded,[erlang]}},\
             {{progress,preloaded}},\
             {{path,[\"{dir_text}\"]}},\
             {{module,lists}},\
             {{apply,{{boot_fixture,mark,[]}}}}]}})), halt(0).",
            boot = boot.display(),
        );
        match Command::new("erl")
            .args(["-noshell", "-eval", &program])
            .output()
        {
            Ok(output) if output.status.success() => {}
            Ok(output) => {
                eprintln!(
                    "SKIP boot: `erl` exited with {}: {}",
                    output.status,
                    String::from_utf8_lossy(&output.stderr)
                );
                return None;
            }
            Err(error) => {
                eprintln!("SKIP boot: cannot run `erl`: {error}");
                return None;
            }
        }
        if !boot.is_file() {
            eprintln!("SKIP boot: `erl` produced no {}", boot.display());
            return None;
        }
        Some(Fixture { dir })
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_dir_all(&self.dir) {
            println!("note: could not remove {}: {error}", self.dir.display());
        }
    }
}

#[test]
fn a_synthetic_boot_script_executes_end_to_end() {
    if booted_runtime().is_none() {
        return;
    }
    let Some(fixture) = Fixture::build("e2e") else {
        return;
    };
    let boot = fixture.dir.join("synthetic.boot");

    // A runtime whose code path holds the fixture (the script's `{path, …}` directive adds
    // it at execution time; the configured path makes the same directory resolvable up
    // front).
    let config = RuntimeConfig::default().with_code_path(fixture.dir.clone());
    let mut rt = Runtime::new(config).expect("the default runtime");
    rt.boot_from_script(&boot)
        .expect("the synthetic script executes end to end");

    // The `apply` step really ran: both modules are cached and the dictionary side effect is
    // observable through the runtime's own `eval`.
    assert!(rt.module("boot_fixture").is_some(), "the applied module loaded");
    assert!(rt.module("lists").is_some(), "the `module` directive loaded lists");
    let marked = rt.eval("boot_fixture:marked()").expect("boot_fixture:marked/0");
    assert_eq!(
        rt.atom_name_of(marked),
        Some("seen"),
        "the apply step's side effect must be observable"
    );
}

// ---------------------------------------------------------------------------
// 6. The CLI really drives the boot-script executor (`-boot FILE`)
// ---------------------------------------------------------------------------

/// Spawns `beam_rt` for a **boot** action (the boot script is only driven when no start
/// instruction follows), with the crash dump disabled so a failing boot does not litter the
/// working directory.
fn beam_rt_boot(args: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_beam_rt"));
    command.env("ERL_CRASH_DUMP_SECONDS", "0");
    command.args(args);
    command
        .output()
        .unwrap_or_else(|error| panic!("spawning beam_rt {args:?} failed: {error}"))
}

#[test]
fn the_cli_drives_the_boot_script_of_a_boot_flag() {
    if booted_runtime().is_none() {
        return;
    }
    let Some(fixture) = Fixture::build("cli") else {
        return;
    };
    // The switch names the boot file **without** its `.boot` suffix — `init` appends it
    // (`erts/preloaded/src/init.erl:1268`).
    let boot = fixture.dir.join("synthetic");
    let output = beam_rt_boot(&["-noshell", "-boot", &boot.to_string_lossy()]);

    // The script's `{apply,{boot_fixture,mark,[]}}` step really ran: its `io:format` line is
    // on the executable's stdout, which nothing but the boot-script executor can produce.
    assert_eq!(
        output.status.code(),
        Some(0),
        "a synthetic boot script exits 0 (stderr {:?})",
        stderr(&output)
    );
    assert!(
        stdout(&output).contains(MARK_LINE),
        "the CLI must run the script's apply step: {:?}",
        stdout(&output)
    );
    assert!(output.stderr.is_empty(), "stderr {:?}", stderr(&output));
}

#[test]
fn the_cli_reports_the_boot_scripts_honest_stop_as_a_typed_error() {
    if booted_runtime().is_none() {
        return;
    }
    if !Path::new(START_BOOT).is_file() {
        eprintln!("SKIP: {START_BOOT} is absent");
        return;
    }
    // The real `start.boot` cannot finish in Milestone 1: the executor stops at the
    // `{apply,{application,load,…}}` directive (no live `application_controller`) with a
    // typed raise the CLI reports as `beam_rt: exit noproc` — exit 1, never a panic, a hang
    // or a silently-wrong success.
    let stem = START_BOOT.trim_end_matches(".boot");
    let output = beam_rt_boot(&["-noshell", "-boot", stem]);
    assert_eq!(
        output.status.code(),
        Some(1),
        "the honest boot stop is a runtime error (stderr {:?})",
        stderr(&output)
    );
    assert_eq!(stdout(&output), "", "the boot stop prints nothing");
    assert!(
        stderr(&output).contains("noproc"),
        "the diagnostic is the boot chain's `noproc`: {:?}",
        stderr(&output)
    );
}

#[test]
fn a_boot_without_the_boot_flag_keeps_the_preloaded_only_drive() {
    if booted_runtime().is_none() {
        return;
    }
    // No `-boot`: the boot action loads the 22 preloaded modules and stops there, as before
    // the boot script was wired in — no output, exit 0.
    let output = beam_rt_boot(&["-noshell"]);
    assert_eq!(output.status.code(), Some(0), "stderr {:?}", stderr(&output));
    assert!(output.stdout.is_empty(), "stdout {:?}", stdout(&output));
    assert!(output.stderr.is_empty(), "stderr {:?}", stderr(&output));
}
