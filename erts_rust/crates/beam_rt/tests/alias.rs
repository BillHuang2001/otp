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

//! Reference **aliases** (`erlang:alias/1`, `erlang:unalias/1`, `erlang:monitor/3`'s
//! `{alias, Mode}`) as `send` destinations, end to end at the **executable** boundary.
//!
//! `src/native/process.rs`'s unit tests pin `alias/1`/`unalias/1` against the runtime's own
//! process table, `src/native/process/monitor.rs`'s pin the `monitor/3` option parser, and
//! `src/context/tests.rs`'s pin alias resolution at the seam. This suite pins what a **user**
//! sees: the built `beam_rt` binary running compiled code (and the `-eval` evaluator), compared
//! with the live system `erl` oracle running the same workload.
//!
//! # The two send seams an alias destination reaches
//!
//! A reference is not a pid, so the destination resolves through the runtime's alias lookup in
//! **two** places, and both are exercised here:
//!
//! * the compiled `Dest ! Msg` **instruction** — [`RuntimeContext::send_message_with_off_heap`]
//!   (`src/context/mod.rs`) — which is what `gen:reply/2`'s `Alias ! {Tag, Reply}`
//!   (`gen.erl:549-553`) compiles to, and therefore what a `gen_server`/`gen:call` reply uses
//!   (the alias fixture's `mon_alias_reply/0` is disassembled to a bare `send` in its worker
//!   closure, and `gen_reply_alias/0` drives `gen:reply/2` directly);
//! * the `erlang:send/2` / `erlang:'!'/2` **BIF** — the `SendBridge` host seam
//!   (`src/context/bridges.rs`) — which is `erlang:send(Alias, Msg)` (`mon_alias_send/0`
//!   compiles that to `call_ext erlang:send/2`).
//!
//! # How the cases are compared
//!
//! Every case is a fixture function `Mod:Fun/0` compiled **at test time** with the system
//! `erlc` into one scratch directory under `$TMPDIR` (a safe single `-pa` for both runtimes —
//! it never holds a `lists.beam`, unlike `tests/fixtures/`). It is run by the two runtimes in
//! their customary shapes and the two printed lines are compared:
//!
//! ```none
//! beam_rt -noshell -pa DIR run DIR/<mod>.beam <mod>:<fun>/0   %% prints the result as `~w~n`
//! erl     -noshell -pa DIR -eval 'io:format("~w~n",[<mod>:<fun>()])' -s init stop
//! ```
//!
//! Both sides pass `-noshell` to line up: the oracle prints its OTP banner without it, and
//! this runtime's ERTS-native actions (`-eval`/`-s`/`-run`/bare shell) print the same banner
//! without it, while the `run` action is banner-free by construction (it records the `noshell`
//! init argument).
//!
//! A few cases additionally run on the `-eval` route (`beam_rt -noshell -eval …`), the route
//! `tests/monitor.rs` and `tests/send.rs` use, so both of this runtime's send entry points are
//! covered by the oracle cross-check.
//!
//! A missing `erl`/`erlc` is a **loud** skip, never a silent pass
//! ([`support::skip_loudly`]); every probe child is polled at [`support::PROBE_TIMEOUT`] and
//! reaped, so a blocked probe can never hang the suite.
//!
//! # What agrees, and what deliberately does not
//!
//! [`compiled_reference_alias_flows_match_the_live_oracle`] and
//! [`alias_semantics_on_the_eval_route_match_the_live_oracle`] assert **exact agreement** with
//! the live oracle, and the expected line is also probed from the oracle itself so a stale
//! recording cannot pass.
//!
//! Two known **divergences** are *reported* (both observed values pinned, never asserted
//! away) by [`the_known_divergences_are_reported_with_both_observed_values`]:
//!
//! * a reference that is **not** a live alias — `make_ref() ! x`, or a send to an alias that was
//!   unaliased — is this runtime's typed `badarg` where the C silently drops the message and
//!   reports the send value (documented at `src/context/alias.rs`);
//! * `erlang:monitor(process, self(), [{alias, _}])` registers an alias here, where the C clears
//!   it for a self-monitor (so a send to it delivers here and is dropped there).
//!
//! `erlang:alias/0` (arity 0, the `alias/1 == alias([])` shorthand `erlang.erl` spells
//! `alias([])`) is served by the runtime-native floor and is one of the **agreeing** cases:
//! both runtimes answer a reference.

// The shared harness is compiled in whole, but this suite uses only its toolchain discovery,
// bounded probe runner and oracle probe, so the unused remainder is expected here.
#[allow(dead_code)]
mod support;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

/// The executable under test.
const BINARY: &str = env!("CARGO_BIN_EXE_beam_rt");

/// The `erl`-facing banner for the loud skip.
const BANNER: &str = "alias";

// ---------------------------------------------------------------------------------------
// The fixture sources (written to the scratch dir and compiled with `erlc`)
// ---------------------------------------------------------------------------------------

/// The `gen_server` callback the `gen_call_*` entries talk to. A **real** `gen_server` is what
/// exercises `gen:do_call/4`'s finite-timeout alias path and `gen:reply/2`'s alias send; the
/// boot path already runs real `gen_server`s (see `tests/boot.rs`'s `logger_server`).
const ALIASGS_SRC: &str = r#"
-module(aliasgs).
-behaviour(gen_server).
-export([init/1, handle_call/3, handle_cast/2, handle_info/2, terminate/2, code_change/3]).

init([]) -> {ok, 0}.
handle_call(get, _From, N) -> {reply, N, N + 1};
handle_call({add, X}, _From, N) -> {reply, N + X, N + X}.
handle_cast(_Msg, N) -> {noreply, N}.
handle_info(_Msg, N) -> {noreply, N}.
terminate(_Reason, _N) -> ok.
code_change(_Old, N, _Extra) -> {ok, N}.
"#;

/// The alias workload. Every entry returns a printable value and catches what it probes, so the
/// process terminates normally and the `run` action prints one line.
const ALIASF_SRC: &str = r#"
-module(aliasf).
-export([mon_alias_reply/0, mon_alias_send/0, mon_alias_consume/0, mon_alias_explicit/0,
         gen_reply_alias/0, gen_call_pid/0, gen_call_name/0,
         alias_option_forms/0, alias_semantics/0, alias_send_compiled/0,
         alias_shapes/0, unalias_shapes/0, mon_three_options/0, mon_two_unchanged/0,
         self_mon_alias/0, non_alias_reference_send/0, alias_zero/0]).

%% monitor/3 with {alias, reply_demonitor}: the monitored worker replies by sending to the
%% monitor's own reference, through the compiled `Alias ! Reply` (a bare `send` instruction).
mon_alias_reply() ->
    P = spawn(fun() -> receive {go, Ref} -> Ref ! replied end end),
    Ref = erlang:monitor(process, P, [{alias, reply_demonitor}]),
    P ! {go, Ref},
    receive X -> X end.

%% Same, but the reply goes through the `erlang:send/2` BIF (`call_ext erlang:send/2`).
mon_alias_send() ->
    P = spawn(fun() -> receive {go, Ref} -> erlang:send(Ref, replied) end end),
    Ref = erlang:monitor(process, P, [{alias, reply_demonitor}]),
    P ! {go, Ref},
    receive X -> X end.

%% A reply_demonitor alias is consumed by the reply it carried: a later `unalias/1` is false.
mon_alias_consume() ->
    P = spawn(fun() -> receive {go, Ref} -> Ref ! replied end end),
    Ref = erlang:monitor(process, P, [{alias, reply_demonitor}]),
    P ! {go, Ref},
    Got = receive X -> X end,
    {Got, erlang:unalias(Ref)}.

%% An explicit_unalias alias survives demonitoring the monitor, so the send still arrives.
mon_alias_explicit() ->
    P = spawn(fun() -> receive stop -> ok end end),
    Ref = erlang:monitor(process, P, [{alias, explicit_unalias}]),
    D = erlang:demonitor(Ref),
    Ref ! hi,
    {D, receive X -> X after 100 -> none end}.

%% `gen:reply/2`'s alias send for a hand-written gen loop: the reply goes to the tag's alias
%% reference (`gen.erl:549-553`), which is what `gen_manager`'s do_call waits on.
gen_reply_alias() ->
    Pid = spawn(fun Loop() ->
                    receive {tag, From, Req} -> gen:reply(From, {got, Req}), Loop() end
                end),
    gen:call(Pid, tag, req, 5000).

%% `gen_server:call/2` uses its default finite timeout, so `gen:do_call/4` takes the
%% `{alias, demonitor}` + `[alias | Mref]` path (`gen.erl:251-281`) and the server replies
%% through the alias.
gen_call_pid() ->
    {ok, Pid} = gen_server:start_link(aliasgs, [], []),
    gen_server:call(Pid, get).

%% The same through a registered name (`gen:do_for_proc/2` resolves it to the pid first).
gen_call_name() ->
    {ok, _Pid} = gen_server:start({local, aliasgs_name}, aliasgs, [], []),
    gen_server:call(aliasgs_name, get).

%% Every documented alias/1 option form answers a reference.
alias_option_forms() ->
    [is_reference(alias(O))
     || O <- [[], [explicit_unalias], [reply], [priority], [reply, explicit_unalias]]].

%% alias([reply]) + erlang:send/2: the message arrives and the alias is consumed by it.
alias_semantics() ->
    A = alias([reply]),
    S1 = erlang:send(A, hello),
    M1 = receive X -> X end,
    {is_reference(A), S1, M1, unalias(A)}.

%% A compiled `Alias ! Msg` to an alias created by alias/1; the default explicit_unalias keeps it.
alias_send_compiled() ->
    A = alias([]),
    A ! hi,
    {receive X -> X end, unalias(A)}.

%% The malformed alias/1 shapes are the oracle's badarg.
alias_shapes() ->
    [try alias(O) catch C:E -> {C, E} end || O <- [[bad], foo, {a, b}]].

%% unalias/1: a non-reference is badarg, a plain reference that is no alias is false.
unalias_shapes() ->
    [try unalias(O) catch C:E -> {C, E} end || O <- [42, make_ref()]].

%% The malformed monitor/3 options are the oracle's badarg (a 1-element option, a 3-tuple, a
%% non-list); an option-less monitor/2 is unchanged.
mon_three_options() ->
    [try erlang:monitor(process, self(), O) catch C:E -> {C, E} end
     || O <- [[foo], {tag, x, extra}, foo]].

mon_two_unchanged() ->
    R = erlang:monitor(process, self()),
    {is_reference(R), erlang:demonitor(R)}.

%% DIVERGENCE: the C clears the alias for a self-monitor, so `Ref ! hi` is dropped there and
%% this runtime's registered alias delivers it (see the module docs).
self_mon_alias() ->
    Ref = erlang:monitor(process, self(), [{alias, reply_demonitor}]),
    Ref ! hi,
    receive X -> X after 100 -> none end.

%% DIVERGENCE: the C drops a send to a reference that is not a live alias and reports the send
%% value; this runtime's alias lookup reports its typed badarg.
non_alias_reference_send() ->
    try make_ref() ! x catch C:E -> {C, E} end.

%% erlang:alias/0 is the `alias([])` shorthand; both runtimes answer a reference.
alias_zero() ->
    try is_reference(alias()) catch C:E -> {C, E} end.
"#;

/// The two fixture modules as `(module, source)` — compiled together into one scratch dir.
const FIXTURES: [(&str, &str); 2] = [("aliasgs", ALIASGS_SRC), ("aliasf", ALIASF_SRC)];

// ---------------------------------------------------------------------------------------
// The agreeing cases (`beam_rt run` vs the live oracle's `-eval`)
// ---------------------------------------------------------------------------------------

/// Every case the two runtimes must print identically: the label, the fixture entry point and
/// the exact line both runtimes print for it.
const AGREEMENTS: [(&str, &str, &str, &str); 15] = [
    (
        "monitor/3 {alias, reply_demonitor} + a compiled `Alias ! Reply`",
        "aliasf",
        "mon_alias_reply",
        "replied",
    ),
    (
        "monitor/3 {alias, reply_demonitor} + `erlang:send(Alias, Reply)`",
        "aliasf",
        "mon_alias_send",
        "replied",
    ),
    (
        "a reply_demonitor alias is consumed by the reply it carries",
        "aliasf",
        "mon_alias_consume",
        "{replied,false}",
    ),
    (
        "an explicit_unalias alias survives demonitoring its monitor",
        "aliasf",
        "mon_alias_explicit",
        "{true,hi}",
    ),
    (
        "gen:reply/2's alias send for a hand-written gen loop",
        "aliasf",
        "gen_reply_alias",
        "{ok,{got,req}}",
    ),
    (
        "gen_server:call(Pid, Req): the default finite timeout takes the alias path",
        "aliasf",
        "gen_call_pid",
        "0",
    ),
    (
        "gen_server:call(Name, Req): the default finite timeout takes the alias path",
        "aliasf",
        "gen_call_name",
        "0",
    ),
    (
        "erlang:alias/1 option forms all answer a reference",
        "aliasf",
        "alias_option_forms",
        "[true,true,true,true,true]",
    ),
    (
        "alias([reply]) + erlang:send/2 delivers and consumes the alias",
        "aliasf",
        "alias_semantics",
        "{true,hello,hello,false}",
    ),
    (
        "a compiled `Alias ! Msg` to an alias created by alias/1",
        "aliasf",
        "alias_send_compiled",
        "{hi,true}",
    ),
    (
        "the malformed alias/1 shapes are badarg",
        "aliasf",
        "alias_shapes",
        "[{error,badarg},{error,badarg},{error,badarg}]",
    ),
    (
        "unalias/1: a non-reference is badarg, a plain reference is false",
        "aliasf",
        "unalias_shapes",
        "[{error,badarg},false]",
    ),
    (
        "the malformed monitor/3 option shapes are badarg",
        "aliasf",
        "mon_three_options",
        "[{error,badarg},{error,badarg},{error,badarg}]",
    ),
    (
        "an option-less monitor/2 is unchanged",
        "aliasf",
        "mon_two_unchanged",
        "{true,true}",
    ),
    (
        "erlang:alias/0 (the alias([]) shorthand) answers a reference",
        "aliasf",
        "alias_zero",
        "true",
    ),
];

/// The same cases on the `-eval` route, given as `(label, value expression, printed line)`.
/// These exercise the evaluator's own `!` operator and its BIF dispatch of `erlang:send/2`,
/// the two send entry points `tests/send.rs` covers — with an **alias** destination.
const EVAL_AGREEMENTS: [(&str, &str, &str); 3] = [
    (
        "erlang:send/2 to an alias created by alias([reply])",
        "begin A = erlang:alias([reply]), R = erlang:send(A, hi), \
         M = receive X -> X end, {R, M, erlang:unalias(A)} end",
        "{hi,hi,false}",
    ),
    (
        "the `!` operator to an alias created by alias([]) (explicit_unalias survives)",
        "begin A = erlang:alias([]), R = (A ! hi), \
         M = receive X -> X end, {R, M, erlang:unalias(A)} end",
        "{hi,hi,true}",
    ),
    (
        "erlang:alias/1 rejects a malformed option",
        "try alias([bad]) catch C:E -> {C, E} end",
        "{error,badarg}",
    ),
];

// ---------------------------------------------------------------------------------------
// The known divergences (both observed values pinned, never asserted away)
// ---------------------------------------------------------------------------------------

/// Each entry: the label, the fixture entry point, this runtime's current answer and the live
/// oracle's answer for the same workload.
const DIVERGENCES: [(&str, &str, &str, &str, &str); 2] = [
    (
        "monitor/3 {alias, _} on self: an alias is registered here, the C clears it",
        "aliasf",
        "self_mon_alias",
        "hi",
        "none",
    ),
    (
        "a reference that is not a live alias: typed badarg here, a silent drop there",
        "aliasf",
        "non_alias_reference_send",
        "{error,badarg}",
        "x",
    ),
];

// ---------------------------------------------------------------------------------------
// Toolchain discovery + the loud skip
// ---------------------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------------------
// The compiled fixture scratch directory
// ---------------------------------------------------------------------------------------

/// The compiled fixtures in one scratch directory (removed on drop).
struct Fixtures {
    /// The scratch directory; a safe single `-pa` for both runtimes (no `lists.beam`).
    dir: PathBuf,
}

impl Fixtures {
    /// The compiled `<module>.beam` inside the scratch directory.
    fn beam(&self, module: &str) -> PathBuf {
        self.dir.join(format!("{module}.beam"))
    }

    /// The scratch directory as a `&str` for `-pa`.
    fn dir_str(&self) -> &str {
        self.dir
            .to_str()
            .unwrap_or_else(|| panic!("the scratch dir is not UTF-8: {}", self.dir.display()))
    }
}

impl Drop for Fixtures {
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

/// Writes and compiles the fixture modules into one scratch directory, or `None` with a loud
/// skip when the toolchain cannot do it.
fn build_fixtures(erlc: &Path) -> Option<Fixtures> {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    // `$TMPDIR` (via `std::env::temp_dir`); the dir is handed to the oracle as a single `-pa`,
    // so it must not contain a `lists.beam` (this crate's `tests/fixtures/` does; these do not).
    let dir = std::env::temp_dir().join(format!("beam_rt_alias_{}_{n}", std::process::id()));
    if let Err(error) = std::fs::create_dir_all(&dir) {
        support::skip_loudly(BANNER, &format!("cannot prepare {}: {error}", dir.display()));
        return None;
    }
    for (module, source) in FIXTURES {
        if let Err(error) = std::fs::write(dir.join(format!("{module}.erl")), source) {
            support::skip_loudly(BANNER, &format!("cannot write {module}.erl: {error}"));
            return None;
        }
    }
    let names: Vec<String> = FIXTURES.iter().map(|(m, _)| format!("{m}.erl")).collect();
    let output = match Command::new(erlc)
        .arg("-o")
        .arg(&dir)
        .args(&names)
        .current_dir(&dir)
        .stdin(Stdio::null())
        .output()
    {
        Ok(output) => output,
        Err(error) => {
            support::skip_loudly(BANNER, &format!("cannot run `erlc`: {error}"));
            return None;
        }
    };
    if !output.status.success() {
        support::skip_loudly(
            BANNER,
            &format!(
                "`erlc` exited with {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            ),
        );
        return None;
    }
    for (module, _) in FIXTURES {
        let beam = dir.join(format!("{module}.beam"));
        if !beam.is_file() {
            support::skip_loudly(BANNER, &format!("`erlc` produced no {}", beam.display()));
            return None;
        }
    }
    Some(Fixtures { dir })
}

/// The live oracle and the compiled fixtures, or `None` after a loud skip. Every test needs
/// both.
fn fixtures_or_skip() -> Option<(PathBuf, Fixtures)> {
    let Some(oracle) = support::require_oracle(BANNER) else {
        return None;
    };
    let Some(erlc) = resolve_erlc() else {
        support::skip_loudly(BANNER, "no `erlc` available (and none beside $ERTS_ORACLE)");
        return None;
    };
    let fixtures = build_fixtures(&erlc)?;
    Some((oracle, fixtures))
}

// ---------------------------------------------------------------------------------------
// The bounded probes
// ---------------------------------------------------------------------------------------

/// One finished probe: its exit code and trimmed stdout/stderr.
struct Run {
    code: i32,
    out: String,
    err: String,
}

/// Runs `program args` through the shared bounded runner (polled at
/// [`support::PROBE_TIMEOUT`], stdin at `/dev/null`), failing loudly when the probe could not be
/// run to completion.
fn run_bounded(program: &Path, args: &[&str]) -> Run {
    let output = support::run_bounded(program, args).unwrap_or_else(|| {
        panic!(
            "`{}` {args:?} could not be run to completion",
            program.display()
        )
    });
    Run {
        code: output.status.code().unwrap_or(-1),
        out: String::from_utf8_lossy(&output.stdout).trim().to_string(),
        err: String::from_utf8_lossy(&output.stderr).trim().to_string(),
    }
}

/// `beam_rt -noshell -pa DIR run DIR/<mod>.beam <mod>:<fun>/0` — the scheduled-process route,
/// which prints the result as `~w~n`.
///
/// The `run` action is banner-free by construction (it records the `noshell` init argument),
/// so it prints no OTP banner (`Erlang/OTP 30 [erts-17.1] [source] [64-bit]`) and the result
/// line is directly comparable to the oracle's `erl -noshell …` output without loosening the
/// assertion; `-noshell` is kept to mirror that probe.
fn run_scheduled(fixtures: &Fixtures, module: &str, function: &str) -> Run {
    let beam = fixtures.beam(module);
    let beam = beam
        .to_str()
        .unwrap_or_else(|| panic!("the fixture path is not UTF-8: {}", beam.display()));
    let spec = format!("{module}:{function}/0");
    run_bounded(
        Path::new(BINARY),
        &["-noshell", "-pa", fixtures.dir_str(), "run", beam, &spec],
    )
}

/// The `-eval` program both runtimes are given for a value expression: print it with `~w`, then
/// halt.
fn eval_program(value: &str) -> String {
    format!("io:format(\"~w~n\",[{value}]), halt(0).")
}

/// `beam_rt -noshell -eval <expr>` through the built binary.
fn beam_rt_eval(expr: &str) -> Run {
    run_bounded(Path::new(BINARY), &["-noshell", "-eval", expr])
}

/// The live oracle's line for `module:function/0`, in the shape the `run` action prints.
fn oracle_line(oracle: &Path, fixtures: &Fixtures, module: &str, function: &str) -> Option<String> {
    support::oracle_eval(
        oracle,
        Some(&fixtures.dir),
        &eval_program(&format!("{module}:{function}()")),
    )
}

// ---------------------------------------------------------------------------------------
// The tests
// ---------------------------------------------------------------------------------------

/// Every agreeing case prints exactly what the live oracle prints for the same function: the
/// compiled `Alias ! Reply` and the `erlang:send(Alias, …)` BIF both reach the alias owner, a
/// `reply_demonitor` alias is consumed by its reply, `gen:reply/2`'s alias send completes a
/// `gen:call`, a real `gen_server` answers `gen_server:call/2` for a pid **and** a registered
/// name through the finite-timeout alias path, and every `alias/1`/`unalias/1`/`monitor/3`
/// rejection is the oracle's own `badarg`.
///
/// The expected line is asserted for this runtime **and** re-probed from the live oracle, so a
/// stale recording cannot pass.
#[test]
fn compiled_reference_alias_flows_match_the_live_oracle() {
    let Some((oracle, fixtures)) = fixtures_or_skip() else {
        return;
    };
    for (label, module, function, expected) in AGREEMENTS {
        let ours = run_scheduled(&fixtures, module, function);
        assert_eq!(
            ours.code, 0,
            "{label}: `beam_rt run {module}:{function}/0` must exit 0, got {}: {}",
            ours.code, ours.err
        );
        assert!(
            !ours.err.contains("unsupported"),
            "{label}: an alias flow is not a capability gap: {}",
            ours.err
        );
        assert_eq!(ours.out, expected, "{label}: beam_rt's answer");

        let theirs = oracle_line(&oracle, &fixtures, module, function)
            .unwrap_or_else(|| panic!("the live oracle could not answer {module}:{function}/0"));
        assert_eq!(
            theirs, expected,
            "{label}: the recorded value must be the live oracle's answer"
        );
    }
}

/// The three alias cases on the `-eval` route (the route `tests/monitor.rs`/`tests/send.rs`
/// use): this runtime's `-eval` `!` operator and its BIF dispatch of `erlang:send/2` resolve an
/// alias destination identically to the oracle, and a malformed `alias/1` option is `badarg`.
#[test]
fn alias_semantics_on_the_eval_route_match_the_live_oracle() {
    let Some(oracle) = support::require_oracle(BANNER) else {
        return;
    };
    for (label, value, expected) in EVAL_AGREEMENTS {
        let program = eval_program(value);
        let ours = beam_rt_eval(&program);
        assert_eq!(ours.code, 0, "{label}: exit {} stderr {}", ours.code, ours.err);
        assert!(ours.err.is_empty(), "{label}: stderr {}", ours.err);
        assert_eq!(ours.out, expected, "{label}: beam_rt's answer");

        let theirs = support::oracle_eval(&oracle, None, &program)
            .unwrap_or_else(|| panic!("the live oracle could not answer `{value}`"));
        assert_eq!(
            theirs, expected,
            "{label}: the recorded value must be the live oracle's answer"
        );
    }
}

/// The two known divergences, **reported** with both observed values rather than asserted
/// away. Each is a real difference from the C/oracle that the alias wiring deliberately leaves
/// in place (see the module docs); pinning both sides keeps it visible and makes any future
/// change to either side fail loudly and force this list to be revisited.
#[test]
fn the_known_divergences_are_reported_with_both_observed_values() {
    let Some((oracle, fixtures)) = fixtures_or_skip() else {
        return;
    };
    for (label, module, function, our_expected, oracle_expected) in DIVERGENCES {
        let ours = run_scheduled(&fixtures, module, function);
        assert_eq!(
            ours.code, 0,
            "{label}: the probe catches its own failure, so it must exit 0, got {}: {}",
            ours.code, ours.err
        );
        assert_eq!(
            ours.out, our_expected,
            "{label}: beam_rt's current (divergent) answer"
        );

        let theirs = oracle_line(&oracle, &fixtures, module, function)
            .unwrap_or_else(|| panic!("the live oracle could not answer {module}:{function}/0"));
        assert_eq!(theirs, oracle_expected, "{label}: the oracle's answer");
        assert_ne!(
            ours.out, theirs,
            "{label}: no longer divergent — move this case to the agreement list"
        );
    }
}
