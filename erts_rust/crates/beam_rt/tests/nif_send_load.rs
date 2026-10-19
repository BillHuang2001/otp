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

//! `enif_send` from a NIF's **`load` callback** (contract A.3): the message really is delivered.
//!
//! Everything is real: the C sources below are compiled at test time with the host C compiler
//! against the host `erl_nif.h`, the Erlang modules with the host `erlc`, and the run is the
//! **built binary** ([`CARGO_BIN_EXE_beam_rt`]) exactly as a user would drive it. The same
//! `.so`/`.beam` are run under the system `erl` as the oracle, so the pinned answer is the
//! oracle's own.
//!
//! # The route this pins, and why it is not the call route
//!
//! A `load` callback has no calling-process borrow available: the host handed to
//! `beam_abi`'s loader is **moved into** the runtime's NIF registry (the library's `unload`
//! callback runs against it long after `erlang:load_nif/2` returns), so it cannot borrow the
//! runtime a normal NIF call borrows.
//! So the callback's `enif_send` is **queued** on the host and delivered by the loader bridge the
//! moment the callback returns — i.e. still inside `erlang:load_nif/2`, which is where the C also
//! enqueues it, so the message is in the receiver's mailbox before the BIF returns either way.
//!
//! # The two forms, and the C's answers
//!
//! `enif_send(env, &to, NULL, msg)` sends a term that belongs to the **load** env; the
//! `msg_env` form (`enif_alloc_env` → build → `enif_send(env, &to, msg_env, copy)` →
//! `enif_free_env`) sends a term owned by a **message** env, which the C idiom frees immediately
//! after the send — so that form is the sharper one: it pins that the queued term survives
//! `enif_free_env` (the host's one never-reset heap owns it) and still reaches the receiver.
//! Both answer the C's `1` when the message was queued and `0` when it was not
//! (`erl_nif.c:860-1093`); the callback's own answer is kept in a static and read back from
//! Erlang with a NIF call, so the test pins the code **and** the delivery.
//!
//! # The oracle
//!
//! The system `erl` delivers from `load` too, and answers the same `1` — every case here asserts
//! our stdout equals the oracle's, plus the exact value so a missing oracle is still a real
//! assertion.
//!
//! # The honest gaps this suite cannot pin
//!
//! An `unload`/resource-`dtor`-time send is **not** covered: the library is only ever dropped
//! with the runtime (nothing removes a registry entry), so there is no live receiver set and no
//! drain point, and that route keeps the ABI's `0`.
//! A send to a local pid whose process is already dead answers `1` from the load window, where
//! the C answers `0` (the liveness relation lives in the process table the load host cannot
//! reach) — so neither of those is asserted here as a delivered message.
//!
//! # Skip policy
//!
//! Three host facilities are needed (a C compiler, the host `erl_nif.h`, the host `erlc`).
//! When one is missing — or the oracle `erl` is not on `$PATH` — the test prints a **loud**
//! banner and returns; it never silently passes.
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// The `msg_env == NULL` fixture (compiled at test time under the stem/module `load_send_nif`).
///
/// `load/3` reads the pid out of the `load_info` term (that is how `erlang:load_nif/2`'s second
/// argument reaches a `load` callback: `enif_get_local_pid(env, info, &to)`), builds
/// `{from_load, 42}` on the load env and sends it; the C's `1`/`0` is kept in `load_send_rc` for
/// `send_rc/0` to hand back.
const LOAD_SEND_C: &str = r#"
#include <erl_nif.h>

/* The `load` callback's own answer, readable from Erlang afterwards. */
static int load_send_rc = -1;

static int load(ErlNifEnv* env, void** priv, ERL_NIF_TERM info) {
    ErlNifPid to;
    ERL_NIF_TERM msg;
    ERL_NIF_TERM arr[2];
    *priv = NULL;
    /* No pid in load_info: nothing to send to, so report nothing sent. */
    if (!enif_get_local_pid(env, info, &to)) return 0;
    arr[0] = enif_make_atom(env, "from_load");
    arr[1] = enif_make_int(env, 42);
    msg = enif_make_tuple_from_array(env, arr, 2);
    load_send_rc = enif_send(env, &to, NULL, msg);   /* the C's 1 / 0 */
    return 0;
}

static ERL_NIF_TERM send_rc(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    (void) argc; (void) argv;
    return enif_make_int(env, load_send_rc);
}

static ErlNifFunc load_send_nif_funcs[] = {
    {"send_rc", 0, send_rc, 0}
};

ERL_NIF_INIT(load_send_nif, load_send_nif_funcs, load, NULL, NULL, NULL)
"#;

/// The **message-env** fixture (compiled at test time under the stem/module `load_send_menv`).
///
/// The message is built on a fresh `enif_alloc_env()` env that the callback **frees before
/// returning**, exactly the C idiom — so the queued term must survive `enif_free_env` and still
/// reach the receiver.
const LOAD_SEND_MENV_C: &str = r#"
#include <erl_nif.h>

static int load_send_rc = -1;

static int load(ErlNifEnv* env, void** priv, ERL_NIF_TERM info) {
    ErlNifPid to;
    ErlNifEnv* menv;
    ERL_NIF_TERM copy;
    ERL_NIF_TERM arr[2];
    *priv = NULL;
    if (!enif_get_local_pid(env, info, &to)) return 0;
    menv = enif_alloc_env();
    if (menv == NULL) return 0;
    arr[0] = enif_make_atom(menv, "from_load_env");
    arr[1] = enif_make_int(menv, 7);
    copy = enif_make_tuple_from_array(menv, arr, 2);
    load_send_rc = enif_send(env, &to, menv, copy);
    enif_free_env(menv);            /* the C idiom: free right after enif_send */
    return 0;
}

static ERL_NIF_TERM send_rc(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    (void) argc; (void) argv;
    return enif_make_int(env, load_send_rc);
}

static ErlNifFunc load_send_menv_funcs[] = {
    {"send_rc", 0, send_rc, 0}
};

ERL_NIF_INIT(load_send_menv, load_send_menv_funcs, load, NULL, NULL, NULL)
"#;

/// The NIF module for a library stem: `init/2` loads the library (handing it the receiver as the
/// `load_info` term), `send_rc/0` is the stub the library replaces.
fn nif_module_erl(stem: &str) -> String {
    format!(
        r#"%% Generated by `beam_rt/tests/nif_send_load.rs` (never committed).
-module({stem}).
-export([init/2, send_rc/0]).
init(Path, Pid) -> erlang:load_nif(Path, Pid).
send_rc() -> erlang:nif_error(not_loaded).
"#
    )
}

/// The caller. Every `load_send_*` call is an **external** call, which is where the runtime's NIF
/// dispatch lives (a local call is resolved in-machine and would run the stub).
///
/// The four forms differ only in *who receives* and *which library*:
/// `self_send/1` and `menv_self_send/1` make the **loading** process the receiver (the message
/// must already be in its mailbox when `erlang:load_nif/2` returns);
/// `child_send/1` and `menv_child_send/1` send to a child parked in `receive`, so the
/// load-callback delivery has to wake it through the runtime's resume-on-send wake list — that is
/// the sharper evidence. Each answers `{Rc, Delivered}`, where `Rc` is the C's own `enif_send`
/// answer and `Delivered` is `timeout` when nothing arrived.
const LOAD_SEND_CALLER_ERL: &str = r#"%% Generated by `beam_rt/tests/nif_send_load.rs` (never committed).
-module(load_send_caller).
-export([self_send/1, child_send/1, menv_self_send/1, menv_child_send/1]).

%% The loading process is its own receiver: the message must be in its mailbox
%% before `erlang:load_nif/2` returns.
self_send(Path) ->
    ok = load_send_nif:init(Path, self()),
    Rc = load_send_nif:send_rc(),
    Got = receive M -> M after 5000 -> timeout end,
    {Rc, Got}.

%% A parked receiver: the load-callback delivery must wake it through the
%% runtime's resume-on-send wake list.
child_send(Path) ->
    Waiter = park(),
    ok = load_send_nif:init(Path, Waiter),
    Rc = load_send_nif:send_rc(),
    Got = receive {echo, M} -> M after 5000 -> timeout end,
    {Rc, Got}.

menv_self_send(Path) ->
    ok = load_send_menv:init(Path, self()),
    Rc = load_send_menv:send_rc(),
    Got = receive M -> M after 5000 -> timeout end,
    {Rc, Got}.

menv_child_send(Path) ->
    Waiter = park(),
    ok = load_send_menv:init(Path, Waiter),
    Rc = load_send_menv:send_rc(),
    Got = receive {echo, M} -> M after 5000 -> timeout end,
    {Rc, Got}.

%% Spawns the receiver, waits until it is parked in its `receive`, and hands it back.
%% (`Waiter` is bound before the pattern, so the `receive` matches *this* child only.)
park() ->
    Parent = self(),
    Waiter = spawn(fun() ->
        Parent ! {ready, self()},
        receive M -> Parent ! {echo, M} end
    end),
    receive {ready, Waiter} -> Waiter end.
"#;

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

/// The two libraries and the three Erlang modules, in one scratch directory.
struct Fixture {
    /// The scratch directory holding the sources and the compiled artifacts.
    dir: PathBuf,
    /// The compiled `load_send_caller.beam` (the `run` file argument).
    caller: PathBuf,
}

impl Fixture {
    /// The path handed to `erlang:load_nif/2` for the library `stem` (the `.so` **without** its
    /// suffix — the loader appends `.so`).
    fn load_path(&self, stem: &str) -> PathBuf {
        self.dir.join(stem)
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

/// Compiles both C fixtures into `<scratch>/<stem>.so` and the three Erlang modules into
/// `<scratch>/*.beam`, or `None` when a host facility is missing (the caller returns early —
/// **loudly**).
fn build_fixture(label: &str) -> Option<Fixture> {
    let Some(include) = erl_include_dir() else {
        skip(&format!(
            "nif_send_load/{label}: no erl_nif.h include directory found"
        ));
        return None;
    };
    if !cc_available() {
        skip(&format!(
            "nif_send_load/{label}: no C compiler ({} / $CC) available",
            cc()
        ));
        return None;
    }
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "beam_rt_nif_send_load_{label}_{}_{n}",
        std::process::id()
    ));
    if let Err(error) = std::fs::create_dir_all(&dir) {
        skip(&format!(
            "nif_send_load/{label}: cannot prepare {}: {error}",
            dir.display()
        ));
        return None;
    }
    // The C sources, one per library.
    for (stem, source) in [
        ("load_send_nif", LOAD_SEND_C),
        ("load_send_menv", LOAD_SEND_MENV_C),
    ] {
        let path = dir.join(format!("{stem}.c"));
        if let Err(error) = std::fs::write(&path, source) {
            skip(&format!(
                "nif_send_load/{label}: cannot write {}: {error}",
                path.display()
            ));
            return None;
        }
    }
    // The Erlang sources: one NIF module per library plus the caller.
    let erl_sources: [(&str, String); 3] = [
        ("load_send_nif", nif_module_erl("load_send_nif")),
        ("load_send_menv", nif_module_erl("load_send_menv")),
        ("load_send_caller", LOAD_SEND_CALLER_ERL.to_string()),
    ];
    let erl_paths: Vec<PathBuf> = erl_sources
        .iter()
        .map(|(stem, source)| {
            let path = dir.join(format!("{stem}.erl"));
            if let Err(error) = std::fs::write(&path, source) {
                skip(&format!(
                    "nif_send_load/{label}: cannot write {}: {error}",
                    path.display()
                ));
            }
            path
        })
        .collect();
    // The shared objects: `cc -shared -fPIC -I<include> -o <dir>/<stem>.so <dir>/<stem>.c`.
    for stem in ["load_send_nif", "load_send_menv"] {
        let output = Command::new(cc())
            .arg("-shared")
            .arg("-fPIC")
            .arg(format!("-I{}", include.display()))
            .arg("-o")
            .arg(dir.join(format!("{stem}.so")))
            .arg(dir.join(format!("{stem}.c")))
            .output();
        match output {
            Ok(output) if output.status.success() => {}
            Ok(output) => {
                skip(&format!(
                    "nif_send_load/{label}: cc failed for {stem}.c ({}):\n{}",
                    output.status,
                    String::from_utf8_lossy(&output.stderr)
                ));
                return None;
            }
            Err(error) => {
                skip(&format!(
                    "nif_send_load/{label}: could not run the C compiler: {error}"
                ));
                return None;
            }
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
                "nif_send_load/{label}: erlc failed ({}):\n{}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            ));
            return None;
        }
        Err(error) => {
            skip(&format!(
                "nif_send_load/{label}: could not run `erlc`: {error}"
            ));
            return None;
        }
    }
    let caller = dir.join("load_send_caller.beam");
    if !caller.is_file() {
        skip(&format!(
            "nif_send_load/{label}: erlc produced no {}",
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
    let deadline = Instant::now() + Duration::from_secs(60);
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

/// The `-noshell -pa <dir> run <load_send_caller.beam> load_send_caller:<function>/1 "<load-path>"`
/// invocation for `beam_rt`.
fn rust_args(fixture: &Fixture, function: &str, stem: &str) -> Vec<String> {
    vec![
        "-noshell".to_string(),
        "-pa".to_string(),
        fixture.dir.display().to_string(),
        "run".to_string(),
        fixture.caller.display().to_string(),
        format!("load_send_caller:{function}/1"),
        // The `run` argument grammar parses a bare token as an atom and only a quoted token as
        // an Erlang char list; `erlang:load_nif/2` requires a list or a binary.
        format!("\"{}\"", fixture.load_path(stem).display()),
    ]
}

/// The oracle's `-noshell -pa <dir> -eval 'io:format("~p~n",[load_send_caller:<function>("/…")])'`
/// invocation — the same `.so` and `.beam`, under the system `erl`.
fn oracle_args(fixture: &Fixture, function: &str, stem: &str) -> Vec<String> {
    let path = fixture.load_path(stem).display().to_string();
    vec![
        "-noshell".to_string(),
        "-pa".to_string(),
        fixture.dir.display().to_string(),
        "-eval".to_string(),
        format!("io:format(\"~p~n\",[load_send_caller:{function}(\"{path}\")])"),
        "-s".to_string(),
        "init".to_string(),
        "stop".to_string(),
    ]
}

/// Runs the fixture's `function/1` over the library `stem` under `beam_rt`, asserting the binary
/// is the one built.
fn run_rust(fixture: &Fixture, function: &str, stem: &str) -> Run {
    let program = PathBuf::from(env!("CARGO_BIN_EXE_beam_rt"));
    run_bounded(&program, &rust_args(fixture, function, stem)).expect("beam_rt terminates")
}

/// Runs the fixture's `function/1` over the library `stem` under the oracle; `None` when `erl` is
/// not on `$PATH` or the probe could not answer (the caller reports a loud skip).
fn run_oracle(fixture: &Fixture, function: &str, stem: &str) -> Option<String> {
    let Some(oracle) = which_erl() else {
        skip("nif_send_load: no `erl` on PATH for the oracle cross-check");
        return None;
    };
    let run = run_bounded(&oracle, &oracle_args(fixture, function, stem))?;
    assert!(
        run.status == 0,
        "the oracle run of load_send_caller:{function}/1 failed: status={} stderr={}",
        run.status,
        run.stderr
    );
    Some(run.stdout.trim().to_string())
}

// ---------------------------------------------------------------------------------------------
// The tests
// ---------------------------------------------------------------------------------------------
/// **A `load` callback's `enif_send` reaches the loading process.** The receiver is the process
/// running `erlang:load_nif/2`, so the message must already be in its mailbox when the BIF
/// returns and the `receive` that follows it reads it.
///
/// Oracle: `{1,{from_load,42}}`.
#[test]
fn a_load_callback_send_reaches_the_loading_process() {
    let Some(fixture) = build_fixture("self") else {
        return;
    };
    let oracle = run_oracle(&fixture, "self_send", "load_send_nif");
    let run = run_rust(&fixture, "self_send", "load_send_nif");
    assert_eq!(
        run.stdout.trim(),
        "{1,{from_load,42}}",
        "load_send_caller:self_send/1 must report the load callback's enif_send 1 and the \
         delivered message; status={} stderr={}",
        run.status,
        run.stderr
    );
    assert_eq!(run.status, 0, "stderr={}", run.stderr);
    if let Some(oracle) = oracle {
        assert_eq!(
            run.stdout.trim(),
            oracle,
            "the oracle's own answer is the reference"
        );
    }
    fixture.cleanup();
}

/// **A `load` callback's `enif_send` wakes a parked receiver.** The child is already blocked in
/// its `receive` when the library loads, so the delivery has to reach the runtime's
/// resume-on-send wake list — the message is not merely queued behind a busy receiver.
///
/// Oracle: `{1,{from_load,42}}`.
#[test]
fn a_load_callback_send_wakes_a_parked_receiver() {
    let Some(fixture) = build_fixture("child") else {
        return;
    };
    let oracle = run_oracle(&fixture, "child_send", "load_send_nif");
    let run = run_rust(&fixture, "child_send", "load_send_nif");
    assert_eq!(
        run.stdout.trim(),
        "{1,{from_load,42}}",
        "load_send_caller:child_send/1 must report the load callback's enif_send 1 and the \
         echoed message; status={} stderr={}",
        run.status,
        run.stderr
    );
    assert_eq!(run.status, 0, "stderr={}", run.stderr);
    if let Some(oracle) = oracle {
        assert_eq!(
            run.stdout.trim(),
            oracle,
            "the oracle's own answer is the reference"
        );
    }
    fixture.cleanup();
}

/// **A message-env send from the `load` callback survives `enif_free_env`.** The message is built
/// on a fresh `enif_alloc_env()` env that the callback frees before returning — the C idiom — and
/// it still reaches the loading process's mailbox.
///
/// Oracle: `{1,{from_load_env,7}}`.
#[test]
fn a_message_env_send_from_the_load_callback_survives_enif_free_env() {
    let Some(fixture) = build_fixture("menv_self") else {
        return;
    };
    let oracle = run_oracle(&fixture, "menv_self_send", "load_send_menv");
    let run = run_rust(&fixture, "menv_self_send", "load_send_menv");
    assert_eq!(
        run.stdout.trim(),
        "{1,{from_load_env,7}}",
        "load_send_caller:menv_self_send/1 must report the load callback's enif_send 1 and the \
         delivered message-env term; status={} stderr={}",
        run.status,
        run.stderr
    );
    assert_eq!(run.status, 0, "stderr={}", run.stderr);
    if let Some(oracle) = oracle {
        assert_eq!(
            run.stdout.trim(),
            oracle,
            "the oracle's own answer is the reference"
        );
    }
    fixture.cleanup();
}

/// **A message-env send from the `load` callback wakes a parked receiver.** The two sharp edges at
/// once: the term outlives the message env the callback freed, and the delivery reaches a child
/// that is already blocked in its `receive`.
///
/// Oracle: `{1,{from_load_env,7}}`.
#[test]
fn a_message_env_load_send_wakes_a_parked_receiver() {
    let Some(fixture) = build_fixture("menv_child") else {
        return;
    };
    let oracle = run_oracle(&fixture, "menv_child_send", "load_send_menv");
    let run = run_rust(&fixture, "menv_child_send", "load_send_menv");
    assert_eq!(
        run.stdout.trim(),
        "{1,{from_load_env,7}}",
        "load_send_caller:menv_child_send/1 must report the load callback's enif_send 1 and the \
         echoed message-env term; status={} stderr={}",
        run.status,
        run.stderr
    );
    assert_eq!(run.status, 0, "stderr={}", run.stderr);
    if let Some(oracle) = oracle {
        assert_eq!(
            run.stdout.trim(),
            oracle,
            "the oracle's own answer is the reference"
        );
    }
    fixture.cleanup();
}
