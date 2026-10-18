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
//! **`erlang:delete_module/1` / `code:delete/1` end to end**: the delete really reaches the
//! booted **live `code_server` process** (kernel_sup's child, which is what `code:delete/1`
//! calls), the server **survives** it, and the runtime-native `code:*` floor keeps answering
//! afterwards — on the `-eval` route **and** on the scheduled `run` route.
//!
//! # Why the server's survival is the thing under test
//!
//! `code:delete/1` is `code_server`'s `handle_call({delete, Mod})`: it calls
//! `erlang:delete_module(Mod)` and then drops the module's `moddb` record
//! (`lib/kernel/src/code_server.erl:339`). So the delete is issued **by a live server
//! process**, and before `erlang:delete_module/1` was adopted by the runtime's
//! `RuntimeHooks` bridge that call raised inside the server — which killed it, the
//! `kernel_sup` supervisor restarted it to the intensity limit, and the whole node went down.
//! A delete that answers `true` and leaves `whereis(code_server)` a pid is therefore proof that
//! the adoption is wired all the way through the *server*, not just through the BIF.
//!
//! Measured on OTP 29.1.1 / erts-17.1 (re-probed against the live oracle on every run) with a
//! scratch module `mymod` compiled at test time onto a `-pa` directory:
//!
//! | Row (after `code:ensure_loaded(mymod)`) | Oracle |
//! |---|---|
//! | `code:delete(mymod)` | `true` |
//! | `code:is_loaded(mymod)` | `false` |
//! | `code:which(mymod) =/= non_existing` | `true` (the `which`-family stays **path**-based) |
//! | `lists:keyfind(mymod,1,code:all_loaded())` | `false` |
//! | `erlang:module_loaded(mymod)` | `false` |
//! | `is_pid(erlang:whereis(code_server))` | `true` |
//! | `code:ensure_loaded(ordsets)` | `{module,ordsets}` |
//! | `code:delete(mymod)` a second time | `false` (and the server still answers) |
//! | `{code:delete(lists), is_pid(whereis(code_server)), code:which(lists) =/= non_existing}` | `{true,true,true}` |
//!
//! Every one of those rows is exactly what this runtime answers too (asserted below, on both
//! routes where the row is expressible).
//!
//! # The one row that cannot agree, and why it is asserted as our own state
//!
//! The **direct** BIF (`erlang:delete_module(mymod)`) is not the same operation as
//! `code:delete/1` in ERTS: the BIF only *retires* the current code to the module's **old
//! code slot**, and it is the server that then drops the `moddb` record. So the oracle answers
//! `code:is_loaded(mymod)` ⇒ `{file,"…/mymod.beam"}` and keeps the module in
//! `code:all_loaded/0` while `erlang:module_loaded(mymod)` is already `false`. This runtime
//! has **no old-code slot** — `CodeServer::forget_module` really unloads — so it answers
//! `false` and does not list the module. That divergence is a documented consequence of the
//! adoption (`src/context/runtime_hooks.rs`'s `delete_module` rustdoc); the test therefore
//! cross-checks the rows both runtimes must agree on (`module_loaded`, `which`,
//! `whereis(code_server)`, `ensure_loaded`) and pins the two own-state rows against **this**
//! runtime's truthful answer.
//!
//! # Harness conventions
//!
//! The toolchain is discovered **by filesystem only** and a missing one is a **loud skip**
//! (`mod support`); every probe is bounded by [`support::PROBE_TIMEOUT`] and killed/reaped
//! rather than able to hang the suite; the oracle's crash dump is redirected into the scratch
//! directory (`ERL_CRASH_DUMP`), which is removed on drop; and the fixture sources are
//! compiled **at test time** with the system `erlc` into a per-call-unique scratch directory
//! (never committed, never a shared `{pid}`-only name).
//!
//! Nothing preloaded is ever deleted: only the scratch `mymod` and — for the stdlib row — the
//! `lists` module, whose deletion is immediately harmless (a later `lists:` call reloads it,
//! exactly as it does on the oracle).

mod support;

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

// ---------------------------------------------------------------------------------------
// The compiled fixtures
// ---------------------------------------------------------------------------------------

/// The scratch module whose deletion is issued: neither preloaded nor under a sticky
/// directory, so `code:delete/1` accepts it on both runtimes.
const MYMOD_ERL: &str = r#"%% Generated by `beam_rt/tests/code_delete_live.rs` (never committed).
-module(mymod).
-export([id/1]).

id(X) -> X.
"#;

/// The `run`-route fixture: `code:delete/1`'s **server** round trip from inside a scheduled
/// process, then the whole floor matrix, printed between two markers.
///
/// The value is delimited (`marker|…|end`) because the `~p` printer wraps a long term onto a
/// continuation line: the marker pair — not the line — is what the comparison unambiguously
/// extracts (the same rule `tests/a9_native_floor.rs` uses). No value inside the region
/// contains a space, so whitespace normalization cannot hide a difference.
const CDELETE_ERL: &str = r#"%% Generated by `beam_rt/tests/code_delete_live.rs` (never committed).
-module(cdelete).
-export([run/0, del_direct/0]).

run() ->
    {module, mymod} = code:ensure_loaded(mymod),
    Del = code:delete(mymod),
    io:format("marker|delete=~p,is_loaded=~p,which=~p,all=~p,ml=~p,cs=~p,ens=~p|end~n",
              [Del, code:is_loaded(mymod), code:which(mymod) =/= non_existing,
               lists:keyfind(mymod,1,code:all_loaded()),
               erlang:module_loaded(mymod), is_pid(whereis(code_server)),
               code:ensure_loaded(ordsets)]).

del_direct() ->
    {module, mymod} = code:ensure_loaded(mymod),
    Del = erlang:delete_module(mymod),
    io:format("marker|delete=~p,which=~p,ml=~p,cs=~p,ens=~p|end~n",
              [Del, code:which(mymod) =/= non_existing,
               erlang:module_loaded(mymod), is_pid(whereis(code_server)),
               code:ensure_loaded(ordsets)]).
"#;

/// The `-eval`-route matrix, after a `code:delete/1` through the live server. Every element is
/// an oracle row (see the module docs).
const EXPR_CODE_DELETE: &str = "\
{module, mymod} = code:ensure_loaded(mymod), \
io:format(\"marker|delete=~p,is_loaded=~p,which=~p,all=~p,ml=~p,cs=~p,ens=~p|end~n\", \
[code:delete(mymod), code:is_loaded(mymod), code:which(mymod) =/= non_existing, \
lists:keyfind(mymod,1,code:all_loaded()), erlang:module_loaded(mymod), \
is_pid(whereis(code_server)), code:ensure_loaded(ordsets)]), halt(0).";
const CODE_DELETE_EXPECTED: &str =
    "delete=true,is_loaded=false,which=true,all=false,ml=false,cs=true,ens={module,ordsets}";

/// The same matrix through the **direct** BIF. The five rows below are the ones both runtimes
/// answer identically (the old-code rows are pinned separately, as own state).
const EXPR_DIRECT_DELETE: &str = "\
{module, mymod} = code:ensure_loaded(mymod), \
io:format(\"marker|delete=~p,which=~p,ml=~p,cs=~p,ens=~p|end~n\", \
[erlang:delete_module(mymod), code:which(mymod) =/= non_existing, \
erlang:module_loaded(mymod), is_pid(whereis(code_server)), \
code:ensure_loaded(ordsets)]), halt(0).";
const DIRECT_DELETE_EXPECTED: &str = "delete=true,which=true,ml=false,cs=true,ens={module,ordsets}";

/// The two rows where ERTS' old-code slot makes the oracle answer differently: this runtime
/// really unloads, so `is_loaded` is `false` and `all_loaded/0` no longer lists the module
/// (the oracle answers `{file,"…"}` / keeps the entry — see the module docs).
const EXPR_DIRECT_DELETE_OWN_STATE: &str = "\
{module, mymod} = code:ensure_loaded(mymod), erlang:delete_module(mymod), \
io:format(\"marker|is_loaded=~p,all=~p|end~n\", [code:is_loaded(mymod), \
lists:keyfind(mymod,1,code:all_loaded())]), halt(0).";
const DIRECT_DELETE_OWN_STATE_EXPECTED: &str = "is_loaded=false,all=false";

/// A **second** delete has to be answered by the same live server (it is a fresh
/// `gen_server:call`), and the oracle answers `false` for it.
const EXPR_DOUBLE_DELETE: &str = "\
{module, mymod} = code:ensure_loaded(mymod), \
io:format(\"marker|first=~p,second=~p,cs=~p|end~n\", \
[code:delete(mymod), code:delete(mymod), is_pid(whereis(code_server))]), halt(0).";
const DOUBLE_DELETE_EXPECTED: &str = "first=true,second=false,cs=true";

/// The stdlib row from the objective: a `lists` delete is accepted (`true`) and the server
/// stays up; `which/1` is unaffected because it never consults the load state.
const EXPR_DELETE_LISTS: &str = "\
io:format(\"marker|delete=~p,cs=~p,which=~p|end~n\", \
[code:delete(lists), is_pid(whereis(code_server)), code:which(lists) =/= non_existing]), \
halt(0).";
const DELETE_LISTS_EXPECTED: &str = "delete=true,cs=true,which=true";

// ---------------------------------------------------------------------------------------
// Running the two runtimes
// ---------------------------------------------------------------------------------------

/// Runs `program args` bounded by [`support::PROBE_TIMEOUT`], failing loudly when it cannot run
/// to completion (a probe that blocks is killed and reaped, never left to hang the suite).
fn run(program: &Path, args: &[&str]) -> Output {
    support::run_bounded(program, args).unwrap_or_else(|| {
        panic!(
            "`{}` {args:?} could not be run to completion",
            program.display()
        )
    })
}

/// `<beam_rt> -noshell -pa dir -eval expr` through the built binary (the `-pa` entry comes
/// before `-eval`, so the code path is in place when the expression runs).
fn beam_rt_eval(code_dir: &Path, expr: &str) -> Output {
    let dir = code_dir.display().to_string();
    run(
        Path::new(env!("CARGO_BIN_EXE_beam_rt")),
        &["-noshell", "-pa", &dir, "-eval", expr],
    )
}

/// `<beam_rt> -noshell -pa dir run <file.beam> <m>:<f>/<a>` — the **scheduled** route, which
/// runs the fixture as a real process inside the scheduler.
fn beam_rt_run(code_dir: &Path, file: &Path, entry: &str) -> Output {
    let dir = code_dir.display().to_string();
    let file = file.display().to_string();
    run(
        Path::new(env!("CARGO_BIN_EXE_beam_rt")),
        &["-noshell", "-pa", &dir, "run", &file, entry],
    )
}

/// The oracle's answer to the same `-eval` probe (`erl -noshell -pa dir -eval expr -s init
/// stop`), or `None` when the probe could not answer at all.
fn oracle_eval(oracle: &Path, code_dir: &Path, expr: &str) -> Option<String> {
    support::oracle_eval(oracle, Some(code_dir), expr)
}

/// The oracle's answer to the **scheduled** row: `erl -noshell -pa dir -run cdelete run -s init
/// stop`.
///
/// The oracle's `-run` gives the arguments as strings and the node is kept from running for
/// ever by `-s init stop`; `ERL_CRASH_DUMP` keeps a crash dump out of the repository tree.
fn oracle_run(oracle: &Path, code_dir: &Path, module: &str, function: &str) -> Output {
    let dir = code_dir.display().to_string();
    let crash_dump = code_dir.join("erl_crash.dump");
    let crash_dump = crash_dump.display().to_string();
    support::run_bounded_with_env(
        oracle,
        &[
            "-noshell", "-pa", &dir, "-run", module, function, "-s", "init", "stop",
        ],
        &[("ERL_CRASH_DUMP", Some(&crash_dump))],
    )
    .unwrap_or_else(|| panic!("the oracle could not run `-run {module} {function}`"))
}

/// The probe's marked region (`marker|…|end`), with **all** whitespace removed.
///
/// The `~p` printer wraps a long value onto a continuation line (and, when it wraps, prints
/// `{module, ordsets}` with a space the oracle's own layout may not have), and the oracle may
/// interleave its own `=ERROR REPORT====` blocks, so neither the whole stdout nor a single line
/// is the value: the **marker pair** delimits it. Dropping every whitespace run is sound
/// because every value compared in these regions is whitespace-free (atoms, booleans and
/// `{Module,Atom}` tuples — no string, path or binary).
fn marked(stdout: &str) -> String {
    let Some(start) = stdout.find("marker|") else {
        panic!("the probe printed no `marker|` region; stdout:\n{stdout}");
    };
    let rest = &stdout[start + "marker|".len()..];
    let Some(end) = rest.find("|end") else {
        panic!("the probe's `marker|` region is unterminated; stdout:\n{stdout}");
    };
    rest[..end].split_whitespace().collect::<Vec<_>>().join("")
}

/// The trimmed stdout of a finished run, asserting it exited `0` and was silent on stderr.
fn printed(answer: &Output, what: &str) -> String {
    assert_eq!(
        answer.status.code(),
        Some(0),
        "{what}: the run must exit 0 — stderr:\n{}",
        String::from_utf8_lossy(&answer.stderr)
    );
    assert!(
        answer.stderr.is_empty(),
        "{what}: a successful run must be silent on stderr — {}",
        String::from_utf8_lossy(&answer.stderr)
    );
    String::from_utf8_lossy(&answer.stdout).to_string()
}

/// Asserts this runtime's `-eval` probe prints exactly `expected` in its marked region, and
/// that the live oracle prints the same region.
///
/// The oracle's stdout may also carry async `=ERROR REPORT====` blocks (a retired module it is
/// asked to purge, say): the marker pair is what makes the oracle's *value* comparable.
fn assert_region_agrees(
    oracle: &Path,
    code_dir: &Path,
    expr: &str,
    expected: &str,
    what: &str,
) {
    let ours = printed(&beam_rt_eval(code_dir, expr), &format!("beam_rt: {what}"));
    assert_eq!(marked(&ours), expected, "beam_rt: {what}");
    match oracle_eval(oracle, code_dir, expr) {
        Some(theirs) => assert_eq!(
            marked(&theirs),
            expected,
            "the system erl must answer `{expected}` for {what}; it printed:\n{theirs}"
        ),
        None => eprintln!("warning: the oracle could not answer `{what}` — not compared"),
    }
}

// ---------------------------------------------------------------------------------------
// The compiled fixture directory
// ---------------------------------------------------------------------------------------

/// The compiled fixture in one scratch directory (removed on drop).
struct Fixture {
    dir: PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_dir_all(&self.dir) {
            eprintln!(
                "warning: could not remove the fixture dir {}: {error}",
                self.dir.display()
            );
        }
    }
}

/// The compiler: a sibling of the resolved oracle first (the same installation), then `$PATH`.
fn resolve_erlc() -> Option<PathBuf> {
    if let Some(dir) = support::resolve_oracle().as_deref().and_then(Path::parent) {
        let candidate = dir.join("erlc");
        if support::is_executable(&candidate) {
            return Some(candidate);
        }
    }
    support::find_on_path("erlc")
}

/// The scratch directory of **one** `build_fixture` call — unique per call (label + pid + a
/// per-process sequence), so two tests can hold their own fixtures at the same time without one
/// `Drop` deleting the other's directory mid-run.
fn scratch_dir(label: &str) -> PathBuf {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "beam_rt_code_delete_{label}_{}_{sequence}",
        std::process::id()
    ))
}

/// Writes and compiles the fixtures into a fresh scratch directory, or `None` after a loud skip
/// when the toolchain cannot do it.
fn build_fixture(erlc: &Path, label: &str) -> Option<Fixture> {
    let dir = scratch_dir(label);
    match std::fs::remove_dir_all(&dir) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            support::skip_loudly(
                "code_delete_live",
                &format!("cannot clear {}: {error}", dir.display()),
            );
            return None;
        }
    }
    if let Err(error) = std::fs::create_dir_all(&dir) {
        support::skip_loudly(
            "code_delete_live",
            &format!("cannot prepare {}: {error}", dir.display()),
        );
        return None;
    }
    let mut sources = Vec::new();
    for (name, source) in [("mymod.erl", MYMOD_ERL), ("cdelete.erl", CDELETE_ERL)] {
        let path = dir.join(name);
        if let Err(error) = std::fs::write(&path, source) {
            support::skip_loudly(
                "code_delete_live",
                &format!("cannot write {}: {error}", path.display()),
            );
            return None;
        }
        sources.push(path);
    }
    let mut command = Command::new(erlc);
    command.arg("-o").arg(&dir);
    for source in &sources {
        command.arg(source);
    }
    match command.output() {
        Ok(output) if output.status.success() => {}
        Ok(output) => {
            support::skip_loudly(
                "code_delete_live",
                &format!(
                    "`erlc` exited with {}: {}",
                    output.status,
                    String::from_utf8_lossy(&output.stderr)
                ),
            );
            return None;
        }
        Err(error) => {
            support::skip_loudly("code_delete_live", &format!("cannot run `erlc`: {error}"));
            return None;
        }
    }
    for module in ["mymod.beam", "cdelete.beam"] {
        if !dir.join(module).is_file() {
            support::skip_loudly(
                "code_delete_live",
                &format!("`erlc` produced no {module} in {}", dir.display()),
            );
            return None;
        }
    }
    Some(Fixture { dir })
}

// ---------------------------------------------------------------------------------------
// The tests
// ---------------------------------------------------------------------------------------

/// A `code:delete/1` of a scratch module — a **live `code_server` round trip** on the `-eval`
/// route — answers the oracle's whole matrix, and the server is still a registered pid
/// afterwards.
#[test]
fn code_delete_through_the_live_server_on_the_eval_route_keeps_it_and_the_floor_alive() {
    let Some(oracle) = support::require_oracle("code_delete_live") else {
        return;
    };
    let Some(erlc) = resolve_erlc() else {
        support::skip_loudly("code_delete_live", "no `erlc` on $PATH");
        return;
    };
    let Some(fixture) = build_fixture(&erlc, "eval") else {
        return;
    };
    assert_region_agrees(
        &oracle,
        &fixture.dir,
        EXPR_CODE_DELETE,
        CODE_DELETE_EXPECTED,
        "code:delete/1 of a scratch module through the live code server",
    );
}

/// The **same live server** answers a second delete (a fresh `gen_server:call`) — the oracle's
/// `false` — which is the strongest evidence that the first one did not take the process down.
#[test]
fn the_live_server_answers_a_second_delete_on_the_eval_route() {
    let Some(oracle) = support::require_oracle("code_delete_live") else {
        return;
    };
    let Some(erlc) = resolve_erlc() else {
        support::skip_loudly("code_delete_live", "no `erlc` on $PATH");
        return;
    };
    let Some(fixture) = build_fixture(&erlc, "double") else {
        return;
    };
    assert_region_agrees(
        &oracle,
        &fixture.dir,
        EXPR_DOUBLE_DELETE,
        DOUBLE_DELETE_EXPECTED,
        "a second code:delete/1 through the live code server",
    );
}

/// The **direct** `erlang:delete_module/1`: the rows both runtimes must agree on are
/// cross-checked against the oracle, and the two rows ERTS' old-code slot makes differ are
/// pinned against this runtime's own (truthful) answer — it really unloads.
#[test]
fn a_direct_delete_module_leaves_the_floor_answering_and_the_server_alive() {
    let Some(oracle) = support::require_oracle("code_delete_live") else {
        return;
    };
    let Some(erlc) = resolve_erlc() else {
        support::skip_loudly("code_delete_live", "no `erlc` on $PATH");
        return;
    };
    let Some(fixture) = build_fixture(&erlc, "direct") else {
        return;
    };
    assert_region_agrees(
        &oracle,
        &fixture.dir,
        EXPR_DIRECT_DELETE,
        DIRECT_DELETE_EXPECTED,
        "the rows erlang:delete_module/1 and the code:* floor agree on",
    );

    // Own state, not an oracle row: this runtime has no old-code slot, so the module is really
    // gone (`is_loaded` ⇒ `false`, `all_loaded/0` no longer lists it) where the oracle keeps a
    // retired image.
    let ours = printed(
        &beam_rt_eval(&fixture.dir, EXPR_DIRECT_DELETE_OWN_STATE),
        "beam_rt: the unload's own state",
    );
    assert_eq!(
        marked(&ours),
        DIRECT_DELETE_OWN_STATE_EXPECTED,
        "this runtime unloads (there is no old-code slot); the oracle retires"
    );
    if let Some(theirs) = oracle_eval(&oracle, &fixture.dir, EXPR_DIRECT_DELETE_OWN_STATE) {
        // Pinned so the *reason* for the divergence is measured here, not remembered: the
        // oracle answers a file record and keeps the module listed.
        let marked = marked(&theirs);
        assert!(
            marked.starts_with("is_loaded={file,"),
            "the oracle retires the image, so is_loaded/1 answers a file record: {marked}"
        );
    }
}

/// The stdlib row from the objective: deleting `lists` is accepted and the server stays up
/// (`which/1` never consults the load state). Both runtimes agree.
#[test]
fn deleting_a_stdlib_module_is_accepted_and_leaves_the_server_alive() {
    let Some(oracle) = support::require_oracle("code_delete_live") else {
        return;
    };
    let Some(erlc) = resolve_erlc() else {
        support::skip_loudly("code_delete_live", "no `erlc` on $PATH");
        return;
    };
    let Some(fixture) = build_fixture(&erlc, "stdlib") else {
        return;
    };
    assert_region_agrees(
        &oracle,
        &fixture.dir,
        EXPR_DELETE_LISTS,
        DELETE_LISTS_EXPECTED,
        "code:delete/1 of the stdlib module lists",
    );
}

/// The **scheduled** (`run`) route: a compiled fixture calls `code:delete/1` from inside a
/// scheduler slice, so the `gen_server` round trip to the live server has to pump and be
/// resumed — and the whole `code:*` floor must answer afterwards. The printed marker line is
/// compared with the oracle's own `-run` row.
#[test]
fn the_run_route_deletes_through_the_live_server_and_the_floor_keeps_answering() {
    let Some(oracle) = support::require_oracle("code_delete_live") else {
        return;
    };
    let Some(erlc) = resolve_erlc() else {
        support::skip_loudly("code_delete_live", "no `erlc` on $PATH");
        return;
    };
    let Some(fixture) = build_fixture(&erlc, "run") else {
        return;
    };
    let beam = fixture.dir.join("cdelete.beam");
    let ours = printed(
        &beam_rt_run(&fixture.dir, &beam, "cdelete:run/0"),
        "beam_rt run: code:delete/1 from a scheduled process",
    );
    assert_eq!(
        marked(&ours),
        CODE_DELETE_EXPECTED,
        "the scheduled route's delete matrix"
    );

    let theirs = oracle_run(&oracle, &fixture.dir, "cdelete", "run");
    assert_eq!(
        theirs.status.code(),
        Some(0),
        "the oracle's -run row must exit 0 — stderr:\n{}",
        String::from_utf8_lossy(&theirs.stderr)
    );
    assert_eq!(
        marked(&String::from_utf8_lossy(&theirs.stdout)),
        CODE_DELETE_EXPECTED,
        "the oracle's scheduled delete matrix"
    );
}
