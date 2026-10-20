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
//! The **oracle replay** for the three committed A.3 (NIF C ABI) fixtures:
//! `tests/fixtures/a3_{schedule,thread_env,resource_stop}.{c,erl}`.
//!
//! Each fixture is a real C NIF plus its Erlang driver, and each was measured against the live
//! system OTP (29.1.1 / erts-17.1). This file keeps them alive — a fixture nobody executes rots —
//! by compiling each one exactly as it was measured (`cc -fPIC -shared -O2 -Wall -Wextra` against
//! the host `erl_nif.h`, then `erlc` on the driver), running it under the live oracle, and asserting
//! the recorded transcript. The raw transcripts are quoted above each assertion (and in the
//! fixtures' own headers); they are the ground truth any Rust implementation must reproduce.
//!
//! The **specification** those transcripts implement — the C line references and the resolved
//! implementation shape per gap — is `../HANDOFF_A3_NIF_HOST.md` §1/§2/§3, a real, readable file in
//! this crate's root.
//!
//! The **in-process ABI assertions** for the same three paths live beside this file in
//! `nif_a3_abi_assertions.rs`: `beam_abi`'s `#[no_mangle]` `enif_*` entry points are plain Rust
//! functions, so the contract measured here on the oracle is asserted there with no `dlopen`, no
//! `.so` and no oracle. This file owns the ground-truth artifacts; that one owns the Rust contract.
//!
//! # Skip-vs-fail policy
//!
//! These tests need `cc`, `erlc`, `erl` and a reachable `erl_nif.h`. If any is missing the test
//! prints a loud `SKIP:` banner naming the reason and returns — **never** a silent pass. With the
//! toolchain present every assertion is hard, and each `#[test]` skips independently.
//!
//! # A probe this file never calls
//!
//! `a3_resource_stop:probe_stop_nostop_type/0` (a resource type opened WITHOUT a `stop`
//! callback) **takes the oracle down** (SIGSEGV, exit 139, 3/3 measured runs): the C only
//! `ASSERT`s the callback exists (`erl_check_io.c:1279`) and a release build compiles the
//! `ASSERT` out, so `erts_resource_stop` calls a NULL function pointer. It is deliberately
//! left uncalled here.
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

use libc::c_int;

// =============================================================================================
// The oracle replay machinery (local to this file on purpose: `tests/common/` is
// shared with every other suite in this tree, and this helper set is fixture-specific)
// =============================================================================================
/// The header the include-directory search keys on.
const HEADER: &str = "erl_nif.h";

/// The committed fixture directory.
fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("fixtures")
}

/// Print a loud skip banner (never a silent pass), test-name + reason.
fn skip(reason: &str) {
    println!("================================================================");
    println!("SKIP: {reason}");
    println!("THIS TEST IS NOT A PASS — the oracle transcript is unverified.");
    println!("================================================================");
}

/// True when `dir` holds a readable `erl_nif.h`.
fn has_header(dir: &Path) -> bool {
    dir.join(HEADER).is_file()
}

/// The `erl` the oracle is: `$ERTS_ORACLE` when it names an executable, else `erl` on `$PATH`.
fn resolve_erl() -> Option<PathBuf> {
    if let Some(value) = std::env::var_os("ERTS_ORACLE") {
        let program = PathBuf::from(&value);
        if program.is_file() {
            return Some(program);
        }
    }
    which_on_path("erl")
}

/// The first executable named `name` on `$PATH`.
fn which_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .filter(|dir| !dir.as_os_str().is_empty())
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

/// The C compiler to use: `$CC` when set, else `cc`.
fn cc() -> String {
    std::env::var("CC").unwrap_or_else(|_| "cc".to_string())
}

/// Is a C compiler available?
fn cc_available() -> bool {
    Command::new(cc())
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

/// Locate the system OTP include directory holding `erl_nif.h`; `None` if unavailable.
///
/// Resolution order — the **first** directory that really contains `erl_nif.h` wins:
///
/// 1. `erl -noshell -eval 'io:format("~s",[code:lib_dir(erts,include)])'`. Some packaged
///    runtimes answer with a path that does not exist (the Nix build on this host answers
///    `…/lib/erlang/**lib**/erts-17.1/include`), so the answer is only accepted when
///    `<dir>/erl_nif.h` is really there;
/// 2. the `erts-*` root derived from the canonicalized `erl` on `$PATH`
///    (`…/erts-<vsn>/bin/erl` → `…/erts-<vsn>/include`) — the reliable one here;
/// 3. `<root>/include` and `<root>/lib/erlang/include` for a small set of standard roots.
fn erl_include_dir(erl: &Path) -> Option<PathBuf> {
    from_erl_eval(erl)
        .or_else(|| from_path_erl())
        .or_else(|| standard_roots().into_iter().find(|root| has_header(root)))
}

/// Step 1: ask the system `erl` for `code:lib_dir(erts, include)`.
fn from_erl_eval(erl: &Path) -> Option<PathBuf> {
    let output = Command::new(erl)
        .args([
            "-noshell",
            "-eval",
            "io:format(\"~s\",[code:lib_dir(erts,include)])",
            "-s",
            "init",
            "stop",
        ])
        .stdin(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let dir = PathBuf::from(text.trim());
    has_header(&dir).then_some(dir)
}

/// Step 2: canonicalize the `erl` on `$PATH` and take the sibling `include` of its `erts-*` dir.
fn from_path_erl() -> Option<PathBuf> {
    let erl = which_on_path("erl")?.canonicalize().ok()?;
    let bin = erl.parent()?;
    let erts = bin.parent()?;
    if bin.file_name()? != "bin" || !erts.file_name()?.to_string_lossy().starts_with("erts-") {
        return None;
    }
    let include = erts.join("include");
    has_header(&include).then_some(include)
}

/// Step 3: the standard installation roots to probe (an explicit `include` dir each).
fn standard_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    for base in [
        "/usr/lib/erlang",
        "/usr/local/lib/erlang",
        "/usr/lib64/erlang",
        "/opt/erlang",
    ] {
        let root = PathBuf::from(base);
        roots.push(root.join("include"));
        roots.push(root.join("lib").join("erlang").join("include"));
    }
    if let Some(top) = std::env::var_os("ERL_TOP") {
        roots.push(PathBuf::from(top).join("include"));
    }
    roots
}

/// Everything the replay needs, or `None` after printing why.
struct Toolchain {
    erl: PathBuf,
    erlc: PathBuf,
    include: PathBuf,
}

/// Resolve the toolchain, printing one loud `SKIP:` banner per missing piece.
fn toolchain(what: &str) -> Option<Toolchain> {
    let Some(erl) = resolve_erl() else {
        skip(&format!(
            "{what}: no system `erl` oracle ($ERTS_ORACLE unset/non-executable, none on $PATH)"
        ));
        return None;
    };
    let Some(erlc) = which_on_path("erlc") else {
        skip(&format!("{what}: no `erlc` on $PATH"));
        return None;
    };
    if !cc_available() {
        skip(&format!("{what}: no C compiler ({} / $CC) available", cc()));
        return None;
    }
    let Some(include) = erl_include_dir(&erl) else {
        skip(&format!(
            "{what}: no OTP include directory holding {HEADER} (tried code:lib_dir(erts,include), \
             the sibling include/ of the canonicalized erl, and the standard roots)"
        ));
        return None;
    };
    Some(Toolchain {
        erl,
        erlc,
        include,
    })
}

/// A unique scratch directory under [`std::env::temp_dir`] (never the repo, never a literal `/tmp`).
fn scratch_dir(tag: &str) -> PathBuf {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("beam_bifs-a3-{tag}-{}-{n}", std::process::id()))
}

/// The one `erl` invocation's observable result.
struct ErlRun {
    eval: String,
    stdout: String,
    stderr: String,
    code: Option<i32>,
}

impl ErlRun {
    /// The stdout lines, trimmed of the trailing newline.
    fn lines(&self) -> Vec<&str> {
        self.stdout.lines().map(str::trim).collect()
    }

    /// Every line, for a panic message.
    fn report(&self) -> String {
        format!(
            "exit={:?}\n--- eval ---\n{}\n--- stdout ---\n{}\n--- stderr ---\n{}",
            self.code, self.eval, self.stdout, self.stderr
        )
    }
}

/// Run one `erl -noshell -pa <dir> -eval <eval> -s init stop`.
///
/// The exit status is **recorded, not required**: the oracle occasionally exits non-zero while
/// still printing every line (a benign `=ERROR REPORT====` on stderr), and the real check is
/// the transcript. A missing line therefore panics with the whole [`ErlRun::report`].
fn run_erl(toolchain: &Toolchain, dir: &Path, eval: &str) -> ErlRun {
    let output = Command::new(&toolchain.erl)
        .arg("-noshell")
        .arg("-pa")
        .arg(dir)
        .arg("-eval")
        .arg(eval)
        .arg("-s")
        .arg("init")
        .arg("stop")
        .stdin(Stdio::null())
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn the oracle '{}': {e}", toolchain.erl.display()));
    ErlRun {
        eval: eval.to_string(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        code: output.status.code(),
    }
}

/// Assert `expected` is one of the printed lines (exact match after trimming).
fn expect_line(run: &ErlRun, expected: &str) {
    if run.lines().contains(&expected) {
        return;
    }
    panic!(
        "the oracle transcript does not contain `{expected}`.\n{}",
        run.report()
    );
}

/// The value of a `label=value` line, or a panic naming the run.
fn value_of<'a>(run: &'a ErlRun, label: &str) -> &'a str {
    let prefix = format!("{label}=");
    for line in run.lines() {
        if let Some(rest) = line.strip_prefix(&prefix) {
            return rest;
        }
    }
    panic!("the oracle transcript has no `{label}=…` line.\n{}", run.report());
}

/// Compile one committed fixture (`.c` → `.so`, `.erl` → `.beam`) into a fresh scratch dir.
///
/// The recipe is the one the fixtures were measured with: `cc -fPIC -shared -O2 -Wall -Wextra
/// -I<inc> -o <tmp>/<stem>.so <fixtures>/<stem>.c`, the `.erl` copied beside it and
/// `erlc -o <tmp>`; `erl -pa <tmp>` then finds both (`-on_load` joins the `.so` path from
/// `code:which/1`'s directory).
fn build_fixture(toolchain: &Toolchain, stem: &str) -> PathBuf {
    let fixtures = fixtures_dir();
    let src = fixtures.join(format!("{stem}.c"));
    let erl_src = fixtures.join(format!("{stem}.erl"));
    assert!(
        src.is_file(),
        "the committed A.3 fixture {stem}.c is missing at {src:?}"
    );
    assert!(
        erl_src.is_file(),
        "the committed A.3 driver {stem}.erl is missing at {erl_src:?}"
    );
    let dir = scratch_dir(stem);
    std::fs::create_dir_all(&dir)
        .unwrap_or_else(|e| panic!("cannot create the scratch dir {dir:?}: {e}"));
    // 1. the shared object
    let so = dir.join(format!("{stem}.so"));
    let output = Command::new(cc())
        .arg("-fPIC")
        .arg("-shared")
        .arg("-O2")
        .arg("-Wall")
        .arg("-Wextra")
        .arg(format!("-I{}", toolchain.include.display()))
        .arg("-o")
        .arg(&so)
        .arg(&src)
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn the C compiler {}: {e}", cc()));
    assert!(
        output.status.success() && so.is_file(),
        "cc failed to build {stem}.so (exit {:?})\n--- stderr ---\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    // 2. the Erlang driver, beside the `.so`
    let copied = dir.join(format!("{stem}.erl"));
    std::fs::copy(&erl_src, &copied)
        .unwrap_or_else(|e| panic!("cannot copy {erl_src:?} to {copied:?}: {e}"));
    let output = Command::new(&toolchain.erlc)
        .arg("-o")
        .arg(&dir)
        .arg(&copied)
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn erlc: {e}"));
    assert!(
        output.status.success(),
        "erlc failed on {stem}.erl (exit {:?})\n--- stderr ---\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    dir
}

// =============================================================================================
// Replay 1 — `a3_schedule`: the `enif_schedule_nif` SUCCESS path
// =============================================================================================
/// Replays the measured oracle transcript of `tests/fixtures/a3_schedule.{c,erl}`.
///
/// Measured verbatim against OTP 29.1.1 / erts-17.1 (one probe per line, a raise captured as
/// `{Class,Reason}`):
///
/// ```text
/// sum={ok,{scheduled,5}}
/// ret={ok,0}
/// loop0={ok,{done,1}}          loop1={ok,{done,2}}        loop3={ok,{done,4}}
/// loop100000={ok,{done,100001}}
/// stack_const=true             (process_info(self(),stack_size) unchanged: 35 -> 35)
/// raise={raised,error,boom}    (class `error`, reason `boom`)
/// badflags={raised,error,badarg}
/// toomany={raised,error,badarg}
/// dirty={ok,{dirty,7}}
/// ttype0={ok,{threads,1,1,0}}  ttype1={ok,{threads,1,2,1}}  ttype2={ok,{threads,1,2,2}}
/// ghost={ok,{ghost,99}}
/// ```
///
/// Readings: a continuation's own value IS the call's result (`sum`), the raw word a
/// scheduling call receives is `0` (`THE_NON_VALUE`, `ret`), a 100 000-entry self-rescheduling
/// chain completes at constant stack (`loop100000`/`stack_const`), `sched_thread_types/1`'s
/// slot 2 is the *continuation's* `enif_thread_type()` — `1` = normal for `flags = 0`, `2` =
/// dirty-CPU for `flags = 1`/`2` while slot 1 (the caller) is always `1` — and a `fun_name`
/// that exists nowhere still works (`ghost`: the name is interned, never resolved).
#[test]
fn the_a3_schedule_fixture_replays_the_measured_oracle_transcript() {
    let Some(toolchain) = toolchain("a3_schedule") else {
        return;
    };
    let dir = build_fixture(&toolchain, "a3_schedule");
    let eval = r#"
Pr = fun(L,F) -> R = (try F() of V -> {ok,V} catch C:Re -> {raised,C,Re} end),
                io:format("~s=~p~n",[L,R]) end,
Pr("sum", fun() -> a3_schedule:sched_sum(2,3) end),
Pr("ret", fun() -> a3_schedule:sched_last_ret() end),
Pr("loop0", fun() -> a3_schedule:sched_loop(0) end),
Pr("loop1", fun() -> a3_schedule:sched_loop(1) end),
Pr("loop3", fun() -> a3_schedule:sched_loop(3) end),
B = element(2,process_info(self(),stack_size)),
Pr("loop100000", fun() -> a3_schedule:sched_loop(100000) end),
A = element(2,process_info(self(),stack_size)),
io:format("stack=~p~n",[{B,A}]),
io:format("stack_const=~p~n",[B =:= A]),
Pr("raise", fun() -> a3_schedule:sched_raise() end),
Pr("badflags", fun() -> a3_schedule:sched_bad_flags() end),
Pr("toomany", fun() -> a3_schedule:sched_too_many_args() end),
Pr("dirty", fun() -> a3_schedule:sched_dirty(7) end),
Pr("ttype0", fun() -> a3_schedule:sched_thread_types(0) end),
Pr("ttype1", fun() -> a3_schedule:sched_thread_types(1) end),
Pr("ttype2", fun() -> a3_schedule:sched_thread_types(2) end),
Pr("ghost", fun() -> a3_schedule:sched_ghost_name(99) end),
io:format("done~n",[]).
"#;
    let run = run_erl(&toolchain, &dir, eval);
    for expected in [
        "sum={ok,{scheduled,5}}",
        "ret={ok,0}",
        "loop0={ok,{done,1}}",
        "loop1={ok,{done,2}}",
        "loop3={ok,{done,4}}",
        "loop100000={ok,{done,100001}}",
        "stack_const=true",
        "raise={raised,error,boom}",
        "badflags={raised,error,badarg}",
        "toomany={raised,error,badarg}",
        "dirty={ok,{dirty,7}}",
        "ttype0={ok,{threads,1,1,0}}",
        "ttype1={ok,{threads,1,2,1}}",
        "ttype2={ok,{threads,1,2,2}}",
        "ghost={ok,{ghost,99}}",
        "done",
    ] {
        expect_line(&run, expected);
    }
    println!("a3_schedule: {}", value_of(&run, "stack"));
    println!("a3_schedule oracle transcript replayed ({} lines)", run.lines().len());
}

// =============================================================================================
// Replay 2 — `a3_thread_env`: the `enif_alloc_env` / `enif_send` message-env idiom
// =============================================================================================
/// Replays the measured oracle transcript of `tests/fixtures/a3_thread_env.{c,erl}`.
///
/// The fixture's own `run/0` drives every probe and prints one line per probe, so the whole
/// measurement is one invocation. Measured verbatim against OTP 29.1.1 / erts-17.1:
///
/// ```text
/// thread_send={{0,1,0},0}          {EnvIsNull, SendCode, ThreadType} ++ [ThreadCreated]
/// thread_send_queue=1              the send landed before the NIF call returned
/// {thread_msg,{from_thread,42,<<"abc">>}}
/// worker_atoms={0,1,1,1,0}         {EnvIsNull, AtomLenOk, AtomTextOk, SendCode, ThreadType}
/// {worker_atom_msg,{worker_atom_check,worker_atom}}
/// main_env={0,1}                   {EnvIsNull, SendCode}
/// {call_path_msg,{from_call,42}}
/// null_env={0,1,1}                 {EnvIsNull, SendCode, ThreadType} with a NULL `env` argument
/// {null_env_msg,{from_call_null,42}}
/// parked_status={status,waiting}    the receiver really is parked before the send
/// thread_send_to={{0,1,0},0}
/// {parked_msg,{parked_got,{from_thread_to_target,42}}}
/// ```
///
/// Readings: on a thread the NIF itself created the oracle's `enif_alloc_env()` answers a
/// **non-NULL** env (`EnvIsNull = 0`) whose atoms live in the ONE global atom table
/// (`worker_atoms` slot 1/2 are both `1`), `enif_send(NULL, …)` answers `1` from there, and
/// `enif_thread_type()` on such a thread is `0` (`ERL_NIF_THR_UNDEFINED`) — while the
/// call-path control reports the scheduler thread's `1`. That send even wakes a receiver
/// parked in `receive` (`parked_msg`).
#[test]
fn the_a3_thread_env_fixture_replays_the_measured_oracle_transcript() {
    let Some(toolchain) = toolchain("a3_thread_env") else {
        return;
    };
    let dir = build_fixture(&toolchain, "a3_thread_env");
    let eval = "a3_thread_env:run(), io:format(\"done~n\",[]).";
    let run = run_erl(&toolchain, &dir, eval);
    for expected in [
        "thread_send={{0,1,0},0}",
        "thread_send_queue=1",
        "{thread_msg,{from_thread,42,<<\"abc\">>}}",
        "worker_atoms={0,1,1,1,0}",
        "{worker_atom_msg,{worker_atom_check,worker_atom}}",
        "main_env={0,1}",
        "{call_path_msg,{from_call,42}}",
        "null_env={0,1,1}",
        "{null_env_msg,{from_call_null,42}}",
        "parked_status={status,waiting}",
        "thread_send_to={{0,1,0},0}",
        "{parked_msg,{parked_got,{from_thread_to_target,42}}}",
        "done",
    ] {
        expect_line(&run, expected);
    }
    // `otp_release=…` is host-dependent; only its presence is asserted.
    assert!(
        run.lines().iter().any(|line| line.starts_with("otp_release=")),
        "the fixture's banner line is missing.\n{}",
        run.report()
    );
}

// =============================================================================================
// Replay 3 — `a3_resource_stop`: the resource `stop` callback and the STOP verdicts
// =============================================================================================
/// Replays the measured oracle verdicts of `tests/fixtures/a3_resource_stop.{c,erl}`.
///
/// Only the **stable** slots are asserted; the probes that measure latency or a genuine race
/// report it as data and are documented as such below. Measured against OTP 29.1.1 /
/// erts-17.1 (tuple shapes are the fixtures' own docs; `Code` bit 0 =
/// `ERL_NIF_SELECT_STOP_CALLED` (1), bit 1 = `…_STOP_SCHEDULED` (2), bit 4 =
/// `…_READ_CANCELLED` (16); `Direct` = the callback's `is_direct_call`, `Thr` =
/// `enif_thread_type()` inside it):
///
/// ```text
/// stop_unknown_fd   {1,1,1,17,1}  -> Code 1 = STOP_CALLED, 1 call, direct 1, the pipe fd, thread 1
/// arm_then_stop     {0,17,1,1,1,2} -> Stop 17 = STOP_CALLED|READ_CANCELLED, 1 call, direct 1, thread 1
/// double_stop       {0,17,1,2,1,1,3} -> the SECOND STOP answers 1 and runs the callback a 2nd time
/// cancel_then_stop  {0,16,1,1,1,1,4} -> Cancel 16 = READ_CANCELLED, then Stop 1 = STOP_CALLED
/// arm_then_stop_fallback  {0,18,…} or {0,2,…} -> the STOP carries STOP_SCHEDULED (2); at return
///                   the callback may (2) or may not (18, plus READ_CANCELLED = 16) have run/// stop_fallback_wait_ms(1000)
///                   {0,18,0,1,0,0,8,901,…} -> code 18, callback deferred to the poller thread:
///                   after the wait it ran once with direct = 0 and thread type 0 (ERL_NIF_THR_UNDEFINED)
/// read_on_pending_stop  ReadOnPending = 2 (STOP_SCHEDULED) — an EXPLICIT race, see below
/// ```
///
/// **Transcript correction against the task's raw numbers** (reported, not hidden): the
/// CANCEL-then-STOP probe's STOP answers `1` (`STOP_CALLED`) — not `17` — because the CANCEL
/// already dropped the read interest, so there is nothing left to cancel; and the repeated
/// STOP answers `1` (the fast track, since the first STOP completed) rather than `2`. Both
/// re-measured values are what the assertions below pin.
///
/// The fallback probes run in their **own** `erl` invocation: the event state is keyed by
/// descriptor and a still-pending deferred stop is stolen by the next `enif_select` on the same
/// fd (`steal_pending_stop_nif`), which is what made an earlier measurement report the previous
/// probe's payload tag.
///
/// `probe_read_on_pending_stop/0` is an explicit race the fixture documents ("both shapes are
/// reported, never asserted"): when the poller has already drained the deferred stop, the READ
/// is a fresh arm and answers `0`. The window is therefore retried a bounded number of times in
/// fresh VMs; every observation must be `0` or `2`, and at least one must be `2` (the pending
/// stop really does steer a later request through `steal_pending_stop_nif`).
#[test]
fn the_a3_resource_stop_fixture_replays_the_measured_oracle_verdicts() {
    let Some(toolchain) = toolchain("a3_resource_stop") else {
        return;
    };
    let dir = build_fixture(&toolchain, "a3_resource_stop");

    // ---- legs 1-4: the synchronous (fast-track / `!wake_poller`) verdicts --------------------
    let eval = r#"
El = fun(I,R) -> try element(I,R) catch _:_ -> no_element end end,
Sl = fun(R,Is) -> list_to_tuple([El(I,R) || I <- Is]) end,
a3_resource_stop:probe_reset(),
R1 = a3_resource_stop:probe_stop_unknown_fd(),
io:format("unknown_fd=~p~n",[Sl(R1,[1,2,3,5])]),
a3_resource_stop:probe_reset(),
R2 = a3_resource_stop:probe_arm_then_stop(),
io:format("arm_then_stop=~p~n",[Sl(R2,[1,2,3,4,5,6])]),
a3_resource_stop:probe_reset(),
R3 = a3_resource_stop:probe_double_stop(),
io:format("double_stop=~p~n",[Sl(R3,[1,2,3,4,5,6,7])]),
a3_resource_stop:probe_reset(),
R4 = a3_resource_stop:probe_cancel_then_stop(),
io:format("cancel_then_stop=~p~n",[Sl(R4,[1,2,3,4,5,6,7])]),
io:format("done~n",[]).
"#;
    let run = run_erl(&toolchain, &dir, eval);
    for expected in [
        // the C's STOP fast track (`erl_check_io.c:1278-1285`): no event state, so the callback
        // runs inline (direct = 1) on the calling scheduler thread (type 1).
        "unknown_fd={1,1,1,1}",
        // the "off" arm with `!wake_poller`: CALL_STOP_AND_RELEASE, code 17, direct = 1.
        "arm_then_stop={0,17,1,1,1,2}",
        // the repeated STOP takes the fast track again -> the callback runs a SECOND time (2 calls).
        "double_stop={0,17,1,2,1,1,3}",
        // CANCEL answered the read's CANCELLED bit (16, no stop bit), the STOP then ran synchronously (1).
        "cancel_then_stop={0,16,1,1,1,1,4}",
        "done",
    ] {
        expect_line(&run, expected);
    }

    // ---- leg 6a: the deferred arm on an fd the kernel poll refuses (`/dev/null`) -------------
    //
    // Measured on this host: {0,18,0,-1,-1,-1,900} most runs and {0,2,1,0,0,7,900} when the
    // poller beats the report. Both are the SAME verdict — the STOP carries the
    // `ERL_NIF_SELECT_STOP_SCHEDULED` bit (2) and NOT `…_STOP_CALLED` (1) — so only that fact is
    // asserted, never the racy call count.
    let eval = r#"
a3_resource_stop:probe_reset(),
R = a3_resource_stop:probe_arm_then_stop_fallback(),
Code = element(2,R),
io:format("fallback_arm=~p~n",
          [{element(1,R), (Code band 2) =/= 0, (Code band 1) =:= 0}]),
io:format("fallback_arm_raw=~p~n",[R]),
io:format("done~n",[]).
"#;
    let run = run_erl(&toolchain, &dir, eval);
    if !run.lines().contains(&"fallback_arm={0,true,true}") {
        panic!(
            "the fallback-fd STOP must carry ERL_NIF_SELECT_STOP_SCHEDULED (2) and not \
             ERL_NIF_SELECT_STOP_CALLED (1) — the deferred `wake_poller` arm of \
             `erl_check_io.c:1485-1517`.\n{}",
            run.report()
        );
    }
    println!("fallback_arm {}", value_of(&run, "fallback_arm_raw"));
    expect_line(&run, "done");

    // ---- leg 6b: the same, with a bounded wait, so the deferred callback's facts are stable ---
    let eval = r#"
a3_resource_stop:probe_reset(),
F = a3_resource_stop:probe_stop_fallback_wait_ms(1000),
Code = element(2,F),
io:format("fallback_wait=~p~n",
          [{element(1,F), (Code band 2) =/= 0, (Code band 1) =:= 0,
            element(4,F), element(5,F), element(6,F), element(7,F), element(8,F),
            element(9,F) >= 0, element(10,F) >= 0}]),
io:format("fallback_wait_raw=~p~n",[F]),
io:format("done~n",[]).
"#;
    let run = run_erl(&toolchain, &dir, eval);
    // {Arm, Scheduled, NotCalled, CallsAfterWait, Direct, ThreadType, Tag, Fd, Waited, Latency}
    if !run.lines().contains(&"fallback_wait={0,true,true,1,0,0,8,901,true,true}") {
        panic!(
            "the deferred fallback STOP must run the callback exactly once from the POLLER \
             thread (is_direct_call = 0, enif_thread_type() = 0 = ERL_NIF_THR_UNDEFINED, payload \
             tag 8, fd 901) and the wait must have observed it.\n{}",
            run.report()
        );
    }
    println!("fallback_wait {}", value_of(&run, "fallback_wait_raw"));
    expect_line(&run, "done");

    // ---- leg 5: a request on an fd whose stop is still pending (an explicit race) -------------
    let eval = r#"
a3_resource_stop:probe_reset(),
R = a3_resource_stop:probe_read_on_pending_stop(),
io:format("read_on_pending=~p~n",[{element(1,R),element(2,R),element(3,R)}]),
io:format("done~n",[]).
"#;
    let mut observed = Vec::new();
    for attempt in 0..6 {
        let run = run_erl(&toolchain, &dir, eval);
        let value = value_of(&run, "read_on_pending").to_string();
        println!("read_on_pending attempt {attempt}: {value}");
        let pending = value
            .trim_start_matches('{')
            .split(',')
            .nth(2)
            .and_then(|slot| slot.trim_end_matches('}').parse::<c_int>().ok())
            .unwrap_or_else(|| {
                panic!(
                    "cannot read the ReadOnPending slot out of `{value}`.\n{}",
                    run.report()
                )
            });
        assert!(
            pending == 0 || pending == 2,
            "a READ on an fd whose stop is pending answered {pending}, but the C's only two shapes \
             are 2 (ERL_NIF_SELECT_STOP_SCHEDULED, the pending stop steered through \
             `steal_pending_stop_nif`, `erl_check_io.c:1318-1337`) and 0 (the poller drained the \
             stop first, so this was a fresh arm).\n{}",
            run.report()
        );
        observed.push(pending);
        if pending == 2 {
            break;
        }
    }
    assert!(
        observed.contains(&2),
        "the deferred-stop arm was never observed in {} fresh-VM attempts: ReadOnPending = {observed:?}. \
         The oracle steers a request on an fd with a pending stop through `steal_pending_stop_nif` \
         and answers ERL_NIF_SELECT_STOP_SCHEDULED (2); if it is never seen the fixture no longer \
         reaches that arm on this host.",
        observed.len()
    );
    println!("a3_resource_stop oracle verdicts replayed (ReadOnPending = {observed:?})");
}
