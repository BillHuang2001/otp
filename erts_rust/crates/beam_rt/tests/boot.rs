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
//! 4. the real committed `bootstrap/bin/start.boot` executes **all 22** of its directives to
//!    completion, ending at the final `{progress,started}` (directive index 21)
//!    ([`the_real_start_boot_runs_to_completion`]);
//! 5. a synthetic boot script — compiled at test time with the system toolchain — executes
//!    end to end and its `{apply, …}` step's side effect is observable
//!    ([`a_synthetic_boot_script_executes_end_to_end`]);
//! 6. the **CLI** really drives that executor (contract A.7/A.9) — on every action but
//!    `-version`, with the explicit `-boot FILE` value or `init`'s default `start`: a
//!    synthetic script's `{apply, …}` reaches stdout through the executable
//!    ([`the_cli_drives_the_boot_script_of_a_boot_flag`]), the real `start.boot` boots all 22
//!    directives to completion and the CLI exits 0 with empty stdout and stderr
//!    ([`the_cli_runs_the_real_boot_script_to_completion`]), and a boot with
//!    **no** `-boot` drives the **default `start` script** all the same
//!    ([`a_boot_without_the_boot_flag_drives_the_default_start_script`]);
//! 7. the real boot leaves a live `logger_server` registered as `logger` — instruction 13's
//!    `{kernelProcess,logger,{logger_server,start_link,[]}}` really starts the child, so
//!    `is_pid(erlang:whereis(logger))` is `true`
//!    ([`the_cli_boot_registers_a_live_logger_process`]).
//!
//! Every test that needs the system Erlang install (the erts application's `ebin` holds the
//! preloaded `.beam`s; the oracle is `erl`) skips **loudly** — a clear `eprintln!` and an
//! early `return` — when it is absent. Nothing here silently passes.
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use beam_rt::boot::bootscript::Instruction;
use beam_rt::{Runtime, RuntimeConfig};

/// The committed `start.boot` of this checkout (the oracle-identical real script).
///
/// `CARGO_MANIFEST_DIR` is `<repo>/erts_rust/crates/beam_rt`, so the repository root (which
/// holds `bootstrap/`) is three ancestors up.
fn start_boot() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .expect("the repository root is three levels above the crate")
        .join("bootstrap/bin/start.boot")
}

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

/// A runtime configured **exactly as the CLI configures a `-boot FILE` invocation**, or
/// `None` — loudly — when the system Erlang install is unavailable or the invocation does not
/// parse.
///
/// The configuration is part of the measurement: `Runtime::with_defaults()` has an *empty*
/// command line, so `init:get_arguments/0` omits the automatic entries the boot chain reads and
/// `application:load/1` fails early (see the `src/native` handoff in `src/boot/CONTEXT.md`).
/// Building the runtime through the CLI's own parser is what makes the in-process executor
/// measure the invocation the CLI — and this suite's CLI test — really runs.
fn boot_cli_runtime(boot_arg: &str) -> Option<Runtime> {
    if booted_runtime().is_none() {
        return None;
    }
    let argv = vec![
        "-noshell".to_string(),
        "-boot".to_string(),
        boot_arg.to_string(),
    ];
    let config = match beam_rt::parse(&argv) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("SKIP: cannot build the `-boot` invocation's configuration ({error})");
            return None;
        }
    };
    match Runtime::new(config) {
        Ok(runtime) => Some(runtime),
        Err(error) => {
            eprintln!("SKIP: no system Erlang install ({error})");
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
// 4. A real boot script: loads modules and runs to completion
// ---------------------------------------------------------------------------

#[test]
fn the_real_start_boot_runs_to_completion() {
    // The CLI-equivalent configuration: the directive positions below are what this suite
    // measures, and a bare `Runtime::with_defaults()` stops three instructions earlier (an
    // empty `init:get_arguments/0` — see `boot_cli_runtime`).
    let Some(mut rt) = boot_cli_runtime("bootstrap/bin/start") else {
        return;
    };
    let path = start_boot();
    if !path.is_file() {
        eprintln!("SKIP: {} is absent", path.display());
        return;
    }

    // The real `start.boot` now runs to completion: `boot_from_script` walks every directive
    // in order — loading each `primLoad` batch, running all three `kernelProcess` directives
    // (whose servers really register) and then `{apply,{c,erlangrc,[]}}`, whose file consult
    // reaches `file_server_2`. On a route that drives a boot script — this `-boot FILE` one,
    // and every other CLI action but `-version` — the name is the booted `kernel_sup`'s own
    // `file_server_2` child (`lib/kernel/src/kernel.erl` starts `{file_server, start_link, []}`,
    // which registers the name — `file_server.erl:266-269`), because `Runtime::new` spawns the
    // runtime's **native** service *unclaimed* whenever
    // `RuntimeConfig::drives_a_boot_script()` holds (`src/runtime.rs`); only a runtime that
    // drives no script at all registers that native service under the name. The consult of
    // `{open,"$HOME/.erlang",[read]}`
    // fails with `{error,enoent}` (this environment has no `$HOME/.erlang`), which
    // `c:erlangrc/0` treats as success, so the executor reaches the final `{progress,started}`
    // directive. It cannot panic or hang — `execute` drives the engine directly (never
    // `run_scheduler`), so a blocking receive is the engine's typed `error::blocked()`, and
    // every scheduler drive is bounded. A watchdog thread is impossible here because
    // `Runtime` is `!Send`; the boundedness is the executor's own by construction.
    let script = rt
        .boot_from_script(&path)
        .expect("the real start.boot runs to completion now that file_server_2 answers");
    assert_eq!(
        script.instructions.len(),
        22,
        "the real start.boot has 22 instructions"
    );
    // The last directive is `{progress, started}` — the observable end of the chain, reached
    // only because `c:erlangrc/0` (the directive before it) ran without error.
    assert!(
        matches!(
            script.instructions.last(),
            Some(Instruction::Progress(marker)) if marker == "started"
        ),
        "the boot chain ends at `{{progress,started}}`, found {:?}",
        script.instructions.last()
    );
    // The last `apply` directive really is instruction 20 — `{apply,{c,erlangrc,[]}}`, whose
    // file consult the `file_server_2` service registered by the booted `kernel_sup` answers on
    // this route, so the chain proceeds to `{progress,started}`.
    assert!(
        matches!(
            script.instructions.get(20),
            Some(Instruction::Apply { mfa })
                if mfa.module == "c" && mfa.function == "erlangrc" && mfa.args.is_empty()
        ),
        "instruction 20 is `{{apply,{{c,erlangrc,[]}}}}`, found {:?}",
        script.instructions.get(20)
    );

    // The `primLoad` batches really loaded, and they are **not** preloaded modules — so this
    // is a genuine script-driven load.
    for module in ["lists", "gen_server", "application_starter", "argparse", "zstd"] {
        assert!(
            rt.module(module).is_some(),
            "{module} must have been loaded by the script"
        );
        assert!(
            !rt.is_preloaded_loaded(module),
            "{module} is not a preloaded module, so the load came from the script"
        );
    }
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

/// Spawns `beam_rt` for a **boot** action (a bare `-noshell`: the preloaded set, then the
/// boot script the CLI resolved — the named `-boot FILE` or, when none was given, `init`'s
/// default `start`), with the crash dump disabled so a failing boot does not litter the
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
fn the_cli_runs_the_real_boot_script_to_completion() {
    if booted_runtime().is_none() {
        return;
    }
    let boot = start_boot();
    if !boot.is_file() {
        eprintln!("SKIP: {} is absent", boot.display());
        return;
    }
    // The real `start.boot` must boot to completion: its three `kernelProcess` directives
    // really run, and its final `{apply,{c,erlangrc,[]}}` directive reaches a `file_server_2`
    // service (on this route the booted `kernel_sup`'s own child — the native service is
    // spawned **unregistered** here) that answers `{open,"$HOME/.erlang",[read]}` with
    // `{error,enoent}` (no `$HOME/.erlang` here), which `c:erlangrc/0` treats as success, so the
    // chain must reach `{progress,started}` and the CLI must exit 0, never a panic, a hang or a
    // silently-wrong success.
    //
    // This pins the completion: the script runs all 22 directives and the CLI exits 0 with
    // empty stdout/stderr (see `M1_GAPS.md` *A.7/A.9 boot*).
    //
    // The switch names the boot file **without** its `.boot` suffix — `init` appends it
    // (`erts/preloaded/src/init.erl:1268`).
    let stem = boot.with_extension("");
    let output = beam_rt_boot(&["-noshell", "-boot", &stem.to_string_lossy()]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "the real boot chain completes (stderr {:?})",
        stderr(&output)
    );
    assert_eq!(stdout(&output), "", "the boot completion prints nothing");
    assert_eq!(stderr(&output), "", "the boot completion reports no error");
}

/// The module the boot-chain survival probe applies: it reports whether the boot chain's
/// `application_controller` kernel process is registered at the point the real script would
/// next need to call it (right after the last `kernelProcess` directive).
const PROBE_ERL: &str = r#"%% Generated by `beam_rt/tests/boot.rs` (never committed).
-module(probe).
-export([run/0]).
run() -> io:format("ac=~p~n", [erlang:whereis(application_controller)]).
"#;

/// The module the boot-logger probe applies: it reports whether the real script's
/// `{kernelProcess,logger,{logger_server,start_link,[]}}` directive (instruction 13) left a live
/// process registered as `logger` by the time the probe runs after the last directive.
const LPROBE_ERL: &str = r#"%% Generated by `beam_rt/tests/boot.rs` (never committed).
-module(lprobe).
-export([run/0]).
run() -> io:format("lg_is_pid=~p~n", [is_pid(erlang:whereis(logger))]).
"#;

/// A scratch directory holding the compiled probe and its `.boot`, removed on drop.
struct Probe {
    dir: PathBuf,
}

impl Drop for Probe {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_dir_all(&self.dir) {
            println!("note: could not remove {}: {error}", self.dir.display());
        }
    }
}

/// Compiles `source` — a module whose name is `tag` — and writes a `<tag>.boot` whose first 20
/// directives are the **real** `start.boot`'s verbatim — every `primLoad` batch and all three
/// `kernelProcess` directives — followed by a code-path entry for the probe and the
/// `{apply,{<tag>,run,[]}}`. The scratch directory is per-`tag`, so two probes can be built
/// concurrently in one test binary. `None` — loudly — when the toolchain is unavailable.
fn build_probe_boot(real_boot: &Path, tag: &str, source: &str) -> Option<Probe> {
    let dir = std::env::temp_dir().join(format!(
        "beam_rt_boot_probe_{tag}_{}",
        std::process::id()
    ));
    if let Err(error) = std::fs::create_dir_all(&dir) {
        eprintln!("SKIP probe: cannot create {}: {error}", dir.display());
        return None;
    }
    let source_path = dir.join(format!("{tag}.erl"));
    if let Err(error) = std::fs::write(&source_path, source) {
        eprintln!("SKIP probe: cannot write {}: {error}", source_path.display());
        return None;
    }
    match Command::new("erlc")
        .arg("+deterministic")
        .arg("-o")
        .arg(&dir)
        .arg(&source_path)
        .output()
    {
        Ok(output) if output.status.success() => {}
        Ok(output) => {
            eprintln!(
                "SKIP probe: `erlc` exited with {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
            return None;
        }
        Err(error) => {
            eprintln!("SKIP probe: cannot run `erlc`: {error}");
            return None;
        }
    }
    let boot = dir.join(format!("{tag}.boot"));
    // Slice and re-encode the real script with the oracle: `lists:sublist(Is, 20)` is its
    // directives 0..=19 (ending at the `application_controller` kernelProcess), to which the
    // probe's code path and `apply` are appended.
    let program = format!(
        "{{ok,B}} = file:read_file(\"{real}\"),\
         {{script,{{N,V}},Is}} = binary_to_term(B),\
         New = {{script,{{N,V}}, lists:sublist(Is, 20) ++ \
             [{{path,[\"{dir}\"]}}, {{apply,{{{tag},run,[]}}}}]}},\
         ok = file:write_file(\"{boot}\", term_to_binary(New)), halt(0).",
        real = real_boot.display(),
        dir = dir.display(),
        tag = tag,
        boot = boot.display(),
    );
    match Command::new("erl").args(["-noshell", "-eval", &program]).output() {
        Ok(output) if output.status.success() => {}
        Ok(output) => {
            eprintln!(
                "SKIP probe: `erl` exited with {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
            return None;
        }
        Err(error) => {
            eprintln!("SKIP probe: cannot run `erl`: {error}");
            return None;
        }
    }
    if !boot.is_file() {
        eprintln!("SKIP probe: the oracle wrote no {}", boot.display());
        return None;
    }
    Some(Probe { dir })
}

#[test]
fn the_boot_chains_kernel_processes_survive_their_directive_processes() {
    if booted_runtime().is_none() {
        return;
    }
    let real = start_boot();
    if !real.is_file() {
        eprintln!("SKIP: {} is absent", real.display());
        return;
    }
    let Some(probe) = build_probe_boot(&real, "probe", PROBE_ERL) else {
        return;
    };
    // The synthetic script is the real one's first 20 directives — through its
    // `{kernelProcess, application_controller, …}` directive — plus the probe. If a kernel
    // process died with its short-lived directive process, `whereis(application_controller)`
    // would be `undefined` here; the real chain must report a live pid (`ac=<0.x.y>`).
    let stem = probe.dir.join("probe");
    let output = beam_rt_boot(&["-noshell", "-boot", &stem.to_string_lossy()]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "the probe boot must run to completion (stderr {:?})",
        stderr(&output)
    );
    let out = stdout(&output);
    assert!(
        out.starts_with("ac=<0."),
        "the `application_controller` kernel process must survive its directive process: {out:?}"
    );
}

#[test]
fn the_cli_boot_registers_a_live_logger_process() {
    // The oracle's behaviour: after the real `bootstrap/bin/start.boot` boots, instruction 13's
    // `{kernelProcess,logger,{logger_server,start_link,[]}}` leaves `logger_server` alive and
    // registered as `logger`, so `is_pid(erlang:whereis(logger))` is `true`.
    //
    // This holds now that the `call_ext` → `erlang:apply/3` seam (`fixed_apply`) runs in-machine:
    // `proc_lib:init_p_do_apply/3`'s `try apply(M,F,A)` enters the child as an ordinary
    // `Step::Enter`, so its blocking `receive` suspends the newly spawned process instead of
    // folding into `error::blocked()`, and `gen_server:init_it` reaches its `register(logger,
    // self())`.
    if booted_runtime().is_none() {
        return;
    }
    let real = start_boot();
    if !real.is_file() {
        eprintln!("SKIP: {} is absent", real.display());
        return;
    }
    let Some(probe) = build_probe_boot(&real, "lprobe", LPROBE_ERL) else {
        return;
    };
    let stem = probe.dir.join("lprobe");
    let output = beam_rt_boot(&["-noshell", "-boot", &stem.to_string_lossy()]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "the lprobe boot must run to completion (stderr {:?})",
        stderr(&output)
    );
    let out = stdout(&output);
    assert!(
        out.contains("lg_is_pid=true"),
        "the real boot must leave a live `logger` process registered (the oracle answers \
         `is_pid(erlang:whereis(logger))` with `true`): {out:?}"
    );
}

#[test]
fn a_boot_without_the_boot_flag_drives_the_default_start_script() {
    if booted_runtime().is_none() {
        return;
    }
    // No `-boot`: the CLI resolves `init`'s own default `start` (`bootfile/2`,
    // `erts/preloaded/src/init.erl:1257-1258`) and drives the installed
    // `<Root>/bin/start.boot`, so the boot action loads the 22 preloaded modules **and** runs
    // all 22 directives to `{progress,started}`. The drive itself prints nothing — no banner
    // (the shell is head-less) and no directive output — so the run stays silent: exit 0 with
    // empty stdout and stderr.
    let output = beam_rt_boot(&["-noshell"]);
    assert_eq!(output.status.code(), Some(0), "stderr {:?}", stderr(&output));
    assert!(output.stdout.is_empty(), "stdout {:?}", stdout(&output));
    assert!(output.stderr.is_empty(), "stderr {:?}", stderr(&output));

    // … and the node really is booted on this route, which is what the default script buys:
    // its instruction 13 (`{kernelProcess,logger,{logger_server,start_link,[]}}`) leaves a live
    // process registered as `logger`, exactly as the oracle's does — a preloaded-only runtime
    // answers `false` here.
    let probe = beam_rt_boot(&[
        "-noshell",
        "-eval",
        "io:format(\"lg=~p~n\",[is_pid(erlang:whereis(logger))])",
    ]);
    assert_eq!(probe.status.code(), Some(0), "probe stderr {:?}", stderr(&probe));
    assert_eq!(
        stdout(&probe),
        "lg=true\n",
        "the default `start` script must leave a live `logger` (stderr {:?})",
        stderr(&probe)
    );
}

/// The loader probe every boot-script route must answer: is the name resolved to a pid?
const LOADER_PROBE: &str = "io:format(\"pl=~p~n\",[is_pid(erlang:whereis(erl_prim_loader))])";
#[test]
fn the_booted_route_registers_a_live_erl_prim_loader_like_the_oracle() {
    // The oracle's own boot registers a live process under `erl_prim_loader` (its C
    // `erts_prim_loader` state machine, `whereis(erl_prim_loader)` = a pid running
    // `erl_prim_loader:loop/3`). This runtime serves the loader's *functions* from the native
    // loader floor and now also owns a `Runnable` service process under the same name,
    // registered on every route that builds a runtime — so the probe agrees on this boot-script
    // (`-noshell -eval`) route: exit 0, empty stderr, exactly `pl=true\n`.
    if booted_runtime().is_none() {
        return;
    }
    eval_probe(LOADER_PROBE, "pl=true\n");
    // Pin the shape against the oracle's own answer, which is the ground truth it mirrors.
    let Some(expected) = oracle(&format!("{LOADER_PROBE}, halt(0).")) else {
        return;
    };
    assert_eq!(
        stdout(&expected),
        "pl=true\n",
        "the oracle must agree that `whereis(erl_prim_loader)` is a pid"
    );
}

#[test]
fn the_version_route_builds_no_runtime_and_has_no_loader_name() {
    // `-version` is the **one** action that never boots: it prints the banner and exits,
    // building no runtime (`cli::drive`'s `Action::Version` arm), so there is no name registry
    // on this route and `erl_prim_loader` — like every other registered name — is simply not a
    // thing here. The honest observable is the banner and nothing else, and a probe smuggled
    // alongside the flag must therefore never run.
    let output = beam_rt_boot(&[
        "-eval",
        "io:format(\"pl=~p~n\",[is_pid(erlang:whereis(erl_prim_loader))])",
        "-version",
    ]);
    assert_eq!(output.status.code(), Some(0), "stderr {:?}", stderr(&output));
    let out = stdout(&output);
    assert!(out.contains("erts-17.1"), "the version route prints the banner: {out:?}");
    assert!(
        !out.contains("pl="),
        "no runtime runs on the `-version` route, so no loader probe can answer: {out:?}"
    );
    assert!(output.stderr.is_empty(), "stderr {:?}", stderr(&output));
}