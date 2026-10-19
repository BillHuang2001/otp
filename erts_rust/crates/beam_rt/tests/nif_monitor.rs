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

//! `enif_monitor_process` / the resource `down` callback / `enif_demonitor_process` end to end
//! (contract A.3): a real C NIF arms a monitor on an **Erlang process** and the resource's `down`
//! callback really fires when that process dies.
//!
//! Everything is real: `tests/fixtures/nif_monitor.c` is compiled at test time with the host C
//! compiler against the host `erl_nif.h`, the two Erlang modules with the host `erlc`, and the run
//! is the **built binary** ([`CARGO_BIN_EXE_beam_rt`]) exactly as a user would drive it. The same
//! `.so`/`.beam` are then run under the system `erl` as the oracle, so every pinned tuple below is
//! the oracle's own measured answer (OTP 29.1.1 / erts-17.1).
//!
//! # Why the fixture is a resource and not just a pid
//!
//! The C keeps a NIF-level monitor **inside the resource object** that armed it
//! (`erts_monitor_tree_insert(&rm->root, &mdp->origin)`, `erl_nif.c:3983-3986`); `down` is fired
//! from that tree (`erts_fire_nif_monitor`, `:3058-3115`) and `enif_demonitor_process` looks the
//! monitor up in it again (`:4019-4052`). A resource type **without** a `down` callback cannot hold
//! monitors at all — `enif_monitor_process` answers `-1` before it even looks at the pid
//! (`:3968-3972`) — so the fixture's type carries one and the Erlang side keeps the resource term
//! alive in a variable across every phase.
//!
//! # The phases (each prints one tuple, so the comparison is plain stdout equality)
//!
//! | function | what it pins | oracle |
//! |---|---|---|
//! | `arm_live/1` | the C's `0` on a live target, and no `down` yet | `{0,0}` |
//! | `arm_then_kill/1` | exactly ONE `down`, handed the pid that was armed on | `{0,1,true}` |
//! | `demonitor_after_arm/1` | `enif_demonitor_process` cancels an armed monitor (the C's `0`), so the death fires nothing | `{0,0}` |
//! | `demonitor_after_target_death/1` | after the fire the monitor is gone, so the C answers `1` | `{1,1}` |
//! | `arm_dead/1` | a pid with no process is the C's `1`, never a fabricated monitor | `{1,0}` |
//! | `arm_undefined/1` | an `enif_set_pid_undefined` target is the C's `1` | `{1,0}` |
//! | `liveness/1` | `enif_is_process_alive`, pinned alongside | `{1,0}` |
//!
//! A fired monitor is **polled for** (`nif_monitor_caller:wait_down/1`, up to 1 s) rather than
//! assumed, because `down` is delivered asynchronously; "the target really is dead" is established
//! with a monitor and its `'DOWN'`, never a sleep.
//!
//! # The measured pre-fix divergence this suite was written against
//!
//! `tools/differential/divergences.md` (§ *A.3 — NIF C ABI: the stateful `enif_*` family*) records
//! that `enif_monitor_process` of a **live** pid cannot be established: the oracle answers `0` (the
//! monitor arms and a `down` fires), this runtime answered `1` (it could not establish it). Every
//! assertion below is therefore an oracle-tuple equality, which names the divergence when it is
//! present instead of degrading into a boolean.
//!
//! # Skip policy
//!
//! Four host facilities are needed (a C compiler, the host `erl_nif.h`, the host `erlc`, the oracle
//! `erl`). When one is missing the test prints a **loud** banner and returns; it never silently
//! passes. When the toolchain is present every assertion is hard.
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// The C fixture: the resource type with a `down` callback, the arm/demonitor pair and the
/// recorded state.
const MONITOR_NIF_C: &str = include_str!("fixtures/nif_monitor.c");

/// The NIF module (one stub per C entry; `init/1` is `erlang:load_nif/2`).
const MONITOR_NIF_ERL: &str = include_str!("fixtures/nif_monitor.erl");

/// The driver module — the phases, in the order the tables above list them.
const MONITOR_CALLER_ERL: &str = include_str!("fixtures/nif_monitor_caller.erl");

/// The `enif_*` symbols the fixture `.so` imports: the runtime must export them as dynamic symbols
/// for `dlopen(RTLD_NOW)` to bind the library, so a missing one is a load failure, not a wrong
/// answer.
///
/// `enif_make_pid` is deliberately **not** among them: it is not one of the `enif_*` slots this
/// workspace's ABI crate exports, so the callback's recorded pid is compared in C
/// (`down_pid_matches/1`) instead of being rebuilt as a term.
const MONITOR_NIF_IMPORTS: &[&str] = &[
    "enif_get_resource",
    "enif_get_local_pid",
    "enif_make_resource",
    "enif_make_int",
    "enif_make_atom",
    "enif_alloc_resource",
    "enif_init_resource_type",
    "enif_monitor_process",
    "enif_demonitor_process",
    "enif_is_process_alive",
    "enif_set_pid_undefined",
];

/// Print a loud skip banner (never a silent pass).
fn skip(reason: &str) {
    println!("================================================================");
    println!("SKIP: {reason}");
    println!("================================================================");
}

// ---------------------------------------------------------------------------------------------
// Locating the host `erl_nif.h`
// ---------------------------------------------------------------------------------------------
/// The header the include-directory search keys on.
const HEADER: &str = "erl_nif.h";

/// Whether `dir` holds a readable `erl_nif.h`.
fn has_header(dir: &Path) -> bool {
    dir.join(HEADER).is_file()
}

/// The first `erl` executable on `PATH`.
fn which_erl() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join("erl"))
        .find(|candidate| candidate.is_file())
}

/// Canonicalize the `erl` on `PATH` and take the sibling `include` of its `erts-*` directory.
fn from_path_erl() -> Option<PathBuf> {
    let erl = which_erl()?.canonicalize().ok()?;
    let bin = erl.parent()?;
    let erts = bin.parent()?;
    if bin.file_name()? != "bin" || !erts.file_name()?.to_string_lossy().starts_with("erts-") {
        return None;
    }
    let include = erts.join("include");
    has_header(&include).then_some(include)
}

/// Search one installation root for an include directory holding `erl_nif.h`.
fn search_root(root: &Path) -> Option<PathBuf> {
    let direct = root.join("include");
    if has_header(&direct) {
        return Some(direct);
    }
    let lib_erlang = root.join("lib").join("erlang");
    for base in [root.to_path_buf(), lib_erlang.clone()] {
        let Ok(entries) = std::fs::read_dir(&base) else {
            continue;
        };
        for entry in entries.flatten() {
            if !entry.file_name().to_string_lossy().starts_with("erts-") {
                continue;
            }
            let include = entry.path().join("include");
            if has_header(&include) {
                return Some(include);
            }
        }
    }
    let include = lib_erlang.join("include");
    has_header(&include).then_some(include)
}

/// The standard installation roots to probe.
fn standard_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(top) = std::env::var_os("ERL_TOP") {
        roots.push(PathBuf::from(top));
    }
    for base in [
        "/usr/lib/erlang",
        "/usr/local/lib/erlang",
        "/usr/lib64/erlang",
        "/opt/erlang",
    ] {
        roots.push(PathBuf::from(base));
    }
    roots
}

/// Locate the host `erl_nif.h` include directory; `None` when it is not installed.
fn erl_include_dir() -> Option<PathBuf> {
    from_path_erl().or_else(|| standard_roots().into_iter().find_map(|root| search_root(&root)))
}

// ---------------------------------------------------------------------------------------------
// The compiled fixture
// ---------------------------------------------------------------------------------------------
/// The C compiler to use: `$CC` when set, else `cc`.
fn cc() -> String {
    std::env::var("CC").unwrap_or_else(|_| "cc".to_string())
}

/// Whether a C compiler is available.
fn cc_available() -> bool {
    Command::new(cc())
        .arg("--version")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

/// A compiled `nif_monitor` library + its driver module, in one scratch directory.
struct Fixture {
    /// The scratch directory holding the sources and the compiled artifacts.
    dir: PathBuf,
    /// The compiled `nif_monitor_caller.beam` (the `run` file argument).
    caller: PathBuf,
}

impl Fixture {
    /// The path handed to `erlang:load_nif/2` (the `.so` **without** its suffix — the loader
    /// appends `.so`).
    fn load_path(&self) -> PathBuf {
        self.dir.join("nif_monitor")
    }

    /// Removes the scratch directory (`cc`/`erlc` output is not source).
    fn cleanup(self) {
        match std::fs::remove_dir_all(&self.dir) {
            Ok(()) => {}
            // The directory is ours and disposable; a failure is reported, not hidden.
            Err(error) => println!("note: could not remove {}: {error}", self.dir.display()),
        }
    }
}

/// A scratch directory that no concurrent test *process* can collide on: the label, the process
/// id, a per-process counter and a clock nonce.
fn scratch_dir(label: &str) -> PathBuf {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.subsec_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!(
        "beam_rt_nif_monitor_{label}_{}_{n}_{nonce}",
        std::process::id()
    ))
}

/// Compiles the C fixture into `<scratch>/nif_monitor.so` and both Erlang modules into
/// `<scratch>/*.beam`, or `None` when a host facility is missing (the caller returns early —
/// **loudly**).
fn build_fixture(label: &str) -> Option<Fixture> {
    let Some(include) = erl_include_dir() else {
        skip(&format!(
            "nif_monitor/{label}: no erl_nif.h include directory found"
        ));
        return None;
    };
    if !cc_available() {
        skip(&format!(
            "nif_monitor/{label}: no C compiler ({} / $CC) available",
            cc()
        ));
        return None;
    }
    let dir = scratch_dir(label);
    let c_src = dir.join("nif_monitor.c");
    if let Err(error) =
        std::fs::create_dir_all(&dir).and_then(|()| std::fs::write(&c_src, MONITOR_NIF_C))
    {
        skip(&format!(
            "nif_monitor/{label}: cannot prepare {}: {error}",
            dir.display()
        ));
        return None;
    }
    let mut erl_paths = Vec::new();
    for (stem, source) in [
        ("nif_monitor", MONITOR_NIF_ERL),
        ("nif_monitor_caller", MONITOR_CALLER_ERL),
    ] {
        let path = dir.join(format!("{stem}.erl"));
        if let Err(error) = std::fs::write(&path, source) {
            skip(&format!(
                "nif_monitor/{label}: cannot write {}: {error}",
                path.display()
            ));
            return None;
        }
        erl_paths.push(path);
    }
    // The shared object: `cc -shared -fPIC -I<include> -o <dir>/nif_monitor.so <dir>/nif_monitor.c`.
    let output = Command::new(cc())
        .arg("-shared")
        .arg("-fPIC")
        .arg(format!("-I{}", include.display()))
        .arg("-o")
        .arg(dir.join("nif_monitor.so"))
        .arg(&c_src)
        .output();
    match output {
        Ok(output) if output.status.success() => {}
        Ok(output) => {
            skip(&format!(
                "nif_monitor/{label}: cc failed ({}):\n{}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            ));
            return None;
        }
        Err(error) => {
            skip(&format!(
                "nif_monitor/{label}: could not run the C compiler: {error}"
            ));
            return None;
        }
    }
    // The Erlang modules: `erlc -o <dir> <dir>/<stem>.erl …`.
    let output = Command::new("erlc")
        .arg("-o")
        .arg(&dir)
        .args(&erl_paths)
        .output();
    match output {
        Ok(output) if output.status.success() => {}
        Ok(output) => {
            skip(&format!(
                "nif_monitor/{label}: erlc failed ({}):\n{}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            ));
            return None;
        }
        Err(error) => {
            skip(&format!(
                "nif_monitor/{label}: could not run `erlc`: {error}"
            ));
            return None;
        }
    }
    let caller = dir.join("nif_monitor_caller.beam");
    if !caller.is_file() {
        skip(&format!(
            "nif_monitor/{label}: erlc produced no {}",
            caller.display()
        ));
        return None;
    }
    Some(Fixture { dir, caller })
}

// ---------------------------------------------------------------------------------------------
// Running the fixture under both runtimes
// ---------------------------------------------------------------------------------------------
/// One run's outcome: its exit status and its stdout/stderr.
struct Run {
    /// The process exit code (`-1` when it was killed by a signal).
    status: i32,
    /// Everything the run wrote to stdout.
    stdout: String,
    /// Everything the run wrote to stderr.
    stderr: String,
}

/// Runs `program args…` to completion (bounded), capturing its streams; `None` on a spawn
/// error or a timeout.
fn run_bounded(program: &Path, args: &[String]) -> Option<Run> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;
    // A phase polls for a fired monitor for at most a second per `wait_down/1`; the bound is
    // generous enough for a loaded host while still guaranteeing the suite cannot hang.
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let output = child.wait_with_output().ok()?;
                return Some(Run {
                    status: status.code().unwrap_or(-1),
                    stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                    stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                });
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(_) => return None,
        }
    }
}

/// The `-noshell -pa <dir> run <nif_monitor_caller.beam> nif_monitor_caller:<function>/1
/// "<load-path>"` invocation for `beam_rt`.
fn rust_args(fixture: &Fixture, function: &str) -> Vec<String> {
    vec![
        "-noshell".to_string(),
        "-pa".to_string(),
        fixture.dir.display().to_string(),
        "run".to_string(),
        fixture.caller.display().to_string(),
        format!("nif_monitor_caller:{function}/1"),
        // The `run` argument grammar parses a bare token as an atom and only a quoted token as
        // an Erlang char list; `erlang:load_nif/2` requires a list or a binary.
        format!("\"{}\"", fixture.load_path().display()),
    ]
}

/// The oracle's `-noshell -pa <dir> -eval 'io:format("~p~n",[nif_monitor_caller:<function>("/…")])'
/// -s init stop` invocation — the same `.so` and `.beam`, under the system `erl`.
///
/// The driver itself prints its tuple and returns `ok`; printing that returned value here makes the
/// oracle's stdout the same two lines this runtime's `run` action prints (the tuple, then `ok`).
fn oracle_args(fixture: &Fixture, function: &str) -> Vec<String> {
    let path = fixture.load_path().display().to_string();
    vec![
        "-noshell".to_string(),
        "-pa".to_string(),
        fixture.dir.display().to_string(),
        "-eval".to_string(),
        format!("io:format(\"~p~n\",[nif_monitor_caller:{function}(\"{path}\")])"),
        "-s".to_string(),
        "init".to_string(),
        "stop".to_string(),
    ]
}

/// Runs the fixture's `function/1` under `beam_rt`, asserting the binary is the one built.
fn run_rust(fixture: &Fixture, function: &str) -> Run {
    let program = PathBuf::from(env!("CARGO_BIN_EXE_beam_rt"));
    run_bounded(&program, &rust_args(fixture, function))
        .unwrap_or_else(|| panic!("beam_rt nif_monitor_caller:{function}/1 must terminate"))
}

/// Runs the fixture's `function/1` under the oracle; `None` when `erl` is not on `$PATH` or the
/// probe could not answer (the caller reports a loud skip).
fn run_oracle(fixture: &Fixture, function: &str) -> Option<String> {
    let Some(oracle) = which_erl() else {
        skip("nif_monitor: no `erl` on PATH for the oracle cross-check");
        return None;
    };
    let run = run_bounded(&oracle, &oracle_args(fixture, function))?;
    assert!(
        run.status == 0,
        "the oracle run of nif_monitor_caller:{function}/1 failed: status={} stderr={}",
        run.status,
        run.stderr
    );
    Some(run.stdout.trim().to_string())
}

/// What `function/1` must print: the pinned tuple, then the `ok` the driver returns (both
/// runtimes print it — the oracle through the `io:format` above, this runtime through the `run`
/// action's own result line).
fn expected(tuple: &str) -> String {
    format!("{tuple}\nok")
}

/// The one assertion every phase makes: the **live oracle** produces the pinned literal, this
/// runtime's stdout is that same literal, and its exit status is `0`.
///
/// The oracle is checked **first**, so a stale literal names itself instead of hiding behind the
/// other side's failure — and the pinned `tuple` is the oracle's own measured answer, so a
/// divergence reads as `{1,0} != {0,0}` and not as two opaque strings.
fn check_phase(fixture: &Fixture, function: &str, tuple: &str) {
    let want = expected(tuple);
    if let Some(oracle) = run_oracle(fixture, function) {
        assert_eq!(
            oracle, want,
            "the pinned literal for nif_monitor_caller:{function}/1 must be the oracle's own answer"
        );
    }
    let run = run_rust(fixture, function);
    assert_eq!(
        run.stdout.trim(),
        want,
        "nif_monitor_caller:{function}/1 must print the oracle's {tuple}; status={} stderr={}",
        run.status,
        run.stderr
    );
    assert_eq!(
        run.status, 0,
        "nif_monitor_caller:{function}/1 must exit 0; stderr={}",
        run.stderr
    );
}

// ---------------------------------------------------------------------------------------------
// The tests
// ---------------------------------------------------------------------------------------------
/// The `.so` imports exactly the `enif_*` symbols the runtime must publish, so a `dlopen` failure
/// is diagnosed instead of guessed at.
#[test]
fn the_binary_exports_the_enif_symbols_the_fixture_imports() {
    // `nm` is not assumed; the real assertion is that each phase's run loads the library at all
    // (a missing symbol means `dlopen` fails and `load_nif/2` reports `load_failed`). This test
    // documents the list and asserts the source actually uses it.
    for symbol in MONITOR_NIF_IMPORTS {
        assert!(
            MONITOR_NIF_C.contains(&format!("{symbol}(")),
            "the fixture must call {symbol}"
        );
    }
    assert!(
        MONITOR_NIF_C.contains("ERL_NIF_INIT(nif_monitor,"),
        "the fixture must declare the nif_monitor module"
    );
    assert!(
        MONITOR_NIF_ERL.contains("-module(nif_monitor)."),
        "the stub module must be nif_monitor"
    );
    assert!(
        !MONITOR_NIF_C.contains("enif_make_pid("),
        "enif_make_pid is not an exported slot; the pid identity is compared in C instead"
    );
}

/// **Arming a monitor on a live process is the C's `0`, and nothing has fired yet.**
///
/// The resource type carries a `down` callback, so the monitor really is installed in the
/// resource's own tree — the ledgered divergence is that this answered `1` ("could not
/// establish") while the oracle answers `0`.
///
/// Oracle: `{0,0}`.
#[test]
fn arming_a_live_process_answers_the_cs_zero() {
    let Some(fixture) = build_fixture("arm_live") else {
        return;
    };
    check_phase(&fixture, "arm_live", "{0,0}");
    fixture.cleanup();
}

/// **Killing the armed target fires `down` exactly once, with the pid that was armed on.**
///
/// The count is polled for (the callback is asynchronous), so this pins the delivery itself, not a
/// lucky ordering: one fire, and `pid.pid` is the recorded target pid (`erl_nif.c:3106`).
///
/// Oracle: `{0,1,true}`.
#[test]
fn killing_the_armed_target_fires_exactly_one_down() {
    let Some(fixture) = build_fixture("arm_then_kill") else {
        return;
    };
    check_phase(&fixture, "arm_then_kill", "{0,1,true}");
    fixture.cleanup();
}

/// **`enif_demonitor_process` cancels an armed monitor** — the C's `0` — and the target's later
/// death then fires nothing (the tree entry is gone, so `erts_fire_nif_monitor` has nothing to
/// fire).
///
/// Oracle: `{0,0}`.
#[test]
fn demonitor_before_the_target_dies_cancels_the_monitor() {
    let Some(fixture) = build_fixture("demonitor_after_arm") else {
        return;
    };
    check_phase(&fixture, "demonitor_after_arm", "{0,0}");
    fixture.cleanup();
}

/// **After the monitor fired, `enif_demonitor_process` answers the C's `1`.**
///
/// The fire itself removes the monitor from the resource's tree, so there is nothing left to
/// identify (`erl_nif.c:4046-4048`); the `down` count stays exactly 1 — a demonitor never
/// re-fires or un-fires a callback.
///
/// Oracle: `{1,1}`.
#[test]
fn demonitor_after_the_target_died_answers_not_found() {
    let Some(fixture) = build_fixture("demonitor_after_target_death") else {
        return;
    };
    check_phase(&fixture, "demonitor_after_target_death", "{1,1}");
    fixture.cleanup();
}

/// **A pid whose process is gone is the C's `1`, never a fabricated monitor.**
///
/// The target is killed and its `'DOWN'` awaited first, so it really has no process; `down` stays
/// `0` — an arm that could not be established must not deliver anything later.
///
/// Oracle: `{1,0}`.
#[test]
fn arming_a_dead_process_answers_one_and_fires_nothing() {
    let Some(fixture) = build_fixture("arm_dead") else {
        return;
    };
    check_phase(&fixture, "arm_dead", "{1,0}");
    fixture.cleanup();
}

/// **An `enif_set_pid_undefined` target is the C's `1`.** The pid check (`erl_nif.c:3975-3976`)
/// precedes any registration, and nothing is recorded.
///
/// Oracle: `{1,0}`.
#[test]
fn arming_an_undefined_pid_answers_one() {
    let Some(fixture) = build_fixture("arm_undefined") else {
        return;
    };
    check_phase(&fixture, "arm_undefined", "{1,0}");
    fixture.cleanup();
}

/// **`enif_is_process_alive` agrees alongside the monitor**, so the two liveness answers the C
/// offers are pinned together: `1` while the process runs, `0` once it is gone.
///
/// Oracle: `{1,0}`.
#[test]
fn the_liveness_answer_agrees() {
    let Some(fixture) = build_fixture("liveness") else {
        return;
    };
    check_phase(&fixture, "liveness", "{1,0}");
    fixture.cleanup();
}
