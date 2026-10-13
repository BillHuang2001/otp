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

//! The **two reference id spaces** and the **provenance bridge**, end to end at the
//! **executable** boundary, cross-checked against the live system `erl` oracle.
//!
//! This runtime mints references from **two independent** monotone `u64` id spaces that both start
//! at `0` and both encode the very same on-heap thing
//! `[make_header(3, REF_SUBTAG), node=0, serial=0, id]`:
//!
//! * [`Runtime::refs`](beam_rt::Runtime) — fed by `erlang:make_ref/0`, the NIF `make_ref`, the
//!   `erlang:send_after/3`/`erlang:start_timer/3` timer references and `seq_trace`;
//! * the `ProcessTable`'s own allocator — fed by every `erlang:monitor/2,3` and `erlang:alias/0,1`
//!   (the `{alias, Mode}` monitor form included).
//!
//! A `send` destination is therefore classified by **provenance** (the term-keyed runtime registry,
//! `src/context/alias.rs`), never by the id the term decodes to — the guard the unit suite's
//! `resolve_send_target_rejects_a_bare_reference_that_collides_with_a_live_alias` pins directly.
//! This suite pins the *observable* answers of every public mint route against every consumer path,
//! with the live oracle as ground truth.
//!
//! # Why the collision itself is not in this file
//!
//! The two id spaces can only be forced onto the same number from **inside** the runtime (the unit
//! test above does it by rewinding `Runtime::refs`). The oracle has **one** id space, so it can never
//! produce a colliding pair and cannot answer that case at all: it is asserted only in the unit
//! suite, never here.
//!
//! # How the cases are compared
//!
//! Every case is a fixture function `refids:Fun/0` compiled **at test time** with the system `erlc`
//! into one scratch directory under `$TMPDIR` (a safe single `-pa` for both runtimes — it never holds
//! a `lists.beam` that shadows stdlib, unlike this crate's `tests/fixtures/`). It is run by the two
//! runtimes in their customary shapes and the two printed lines are compared:
//!
//! ```none
//! beam_rt -pa DIR run DIR/refids.beam refids:function/0     %% prints the result as `~w~n`
//! erl     -noshell -pa DIR -eval 'io:format("~w~n",[refids:function()])' -s init stop
//! ```
//!
//! The mint routes covered are `erlang:make_ref/0`; `erlang:monitor/2` (both the reference it returns
//! **and** the `Ref` inside the delivered `{'DOWN', …}`); `erlang:alias/0`; `erlang:alias/1` for
//! `[]`/`[explicit_unalias]`/`[reply]`/`[priority]`; `erlang:monitor/3` `{alias, Mode}`; and the
//! `erlang:send_after/3` + `erlang:start_timer/3` timer references. The consumer paths are
//! `erlang:is_reference/1`; `erlang:send/2`, `erlang:'!'/2` and the compiled `Dest ! Msg`;
//! `erlang:demonitor/1,2`; `erlang:unalias/1`; a `term_to_binary/1`+`binary_to_term/1` round trip;
//! `=:=`/`==`; and use as a **map** key and an **`ets`** key.
//!
//! # Documented divergences (reported, never asserted away)
//!
//! [`the_known_divergences_are_reported_with_both_observed_values`] pins **both** observed values for
//! each case the two runtimes disagree on. None is introduced by the provenance bridge; they are the
//! long-standing reference hazards this runtime's registries imply:
//!
//! * a `send` to a reference that is **not** a live alias — a bare `make_ref/0`, a `monitor/2`
//!   reference or a timer reference — is this runtime's typed `badarg` where the C silently drops the
//!   message and reports the send value (documented at `src/context/alias.rs`; the provenance bridge
//!   is what keeps a **colliding** bare reference from being *delivered* to an alias owner);
//! * `erlang:monitor(process, self(), [{alias, _}])` registers an alias here where the C clears it for
//!   a self-monitor (documented at `tests/alias.rs`);
//! * `erlang:unalias/1` and `erlang:cancel_timer/1` on a **decoded copy** of a reference
//!   (`binary_to_term(term_to_binary(R))`) miss the term-keyed registries, so they answer `false` /
//!   `badarg` where the C reads the ref thing's payload and answers `true` / the remaining time. This
//!   is the same "identity is the term, not the id" seam the HAZARD-2 unit test pins, and closing it
//!   is owned by `../beam_process/` (unify the two allocators) — *not* by `beam_rt`.
//!
//! A reference **without** a decoded copy (the live mint) is one of the agreeing cases, and a decoded
//! copy of a **live alias** is a working `send` destination on both runtimes (the "unknown
//! provenance falls back to the id the term encodes" arm).
//!
//! A missing `erl`/`erlc` is a **loud** skip, never a silent pass ([`support::skip_loudly`]); every
//! probe child is polled at [`support::PROBE_TIMEOUT`] and reaped, so a blocked probe can never hang
//! the suite.

// The shared harness is compiled in whole, but this suite uses only its toolchain discovery, bounded
// probe runner and oracle probe, so the unused remainder is expected here.
#[allow(dead_code)]
mod support;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

/// The executable under test.
const BINARY: &str = env!("CARGO_BIN_EXE_beam_rt");

/// The `erl`-facing banner for the loud skip.
const BANNER: &str = "reference_ids";

// ---------------------------------------------------------------------------------------
// The fixture source (written to the scratch dir and compiled with `erlc`)
// ---------------------------------------------------------------------------------------

/// The reference-workload fixture. Every entry point returns a printable value (or catches what it
/// probes, so its process terminates normally and the `run` action prints one line), and no entry
/// point prints a pid or a raw reference — only booleans, atoms and the values that must agree
/// between the two runtimes.
const REFIDS_SRC: &str = r#"
-module(refids).
-export([make_ref_consumers/0, monitor_consumers/0, alias_consumers/0, mon_alias_reply/0,
         timer_consumers/0, round_trip/0, key_uses/0, copy_alias_send/0, demonitor_two/0,
         send_bare_ref/0, send_monitor_ref/0, send_timer_ref/0, mon_self_alias/0,
         unalias_copy/0, cancel_timer_copy/0]).

%% --- erlang:make_ref/0 through every consumer path -----------------------
make_ref_consumers() ->
    R = make_ref(),
    Copy = binary_to_term(term_to_binary(R)),
    {is_reference(R), R =:= Copy, R == Copy, erlang:demonitor(R), erlang:unalias(R)}.

%% --- erlang:monitor/2 (and the Ref inside the delivered DOWN) -----------
monitor_consumers() ->
    P = spawn(fun() -> ok end),
    M = erlang:monitor(process, P),
    Copy = binary_to_term(term_to_binary(M)),
    Down = receive {'DOWN', D, process, P, Reason} -> {D =:= M, is_reference(D), Reason}
           after 1000 -> timeout end,
    {is_reference(M), M =:= Copy, Down, erlang:demonitor(M)}.

%% --- alias/0 and alias/1 for every documented option form ---------------
alias_consumers() ->
    A0 = erlang:alias(),
    R0 = is_reference(A0),
    S0 = (A0 ! hi),
    G0 = receive X -> X end,
    U0 = erlang:unalias(A0),
    A1 = erlang:alias([explicit_unalias]),
    R1 = is_reference(A1),
    S1 = erlang:send(A1, hi),
    G1 = receive Y -> Y end,
    U1 = erlang:unalias(A1),
    A2 = erlang:alias([reply]),
    R2 = is_reference(A2),
    S2 = (A2 ! hi),
    G2 = receive Z -> Z end,
    U2 = erlang:unalias(A2),
    A3 = erlang:alias([priority]),
    R3 = is_reference(A3),
    S3 = (A3 ! hi),
    G3 = receive W -> W end,
    U3 = erlang:unalias(A3),
    {R0,S0,G0,U0, R1,S1,G1,U1, R2,S2,G2,U2, R3,S3,G3,U3}.

%% --- monitor/3 {alias, reply_demonitor} + the compiled `Alias ! Reply` ----
mon_alias_reply() ->
    P = spawn(fun() -> receive {go, Ref} -> Ref ! replied end end),
    R = erlang:monitor(process, P, [{alias, reply_demonitor}]),
    P ! {go, R},
    Got = receive X -> X end,
    {is_reference(R), Got, erlang:unalias(R)}.

%% --- timer references: send_after/3 and start_timer/3 --------------------
timer_consumers() ->
    T1 = erlang:send_after(100000, self(), fire),
    I1 = is_reference(T1),
    C1 = erlang:cancel_timer(T1),
    C2 = erlang:cancel_timer(T1),
    Eq = (T1 =:= binary_to_term(term_to_binary(T1))),
    T2 = erlang:start_timer(100000, self(), tick),
    I2 = is_reference(T2),
    C3 = is_integer(erlang:cancel_timer(T2)),
    {I1, is_integer(C1), C2, Eq, I2, C3}.

%% --- the term_to_binary/binary_to_term round trip, every mint route -----
round_trip() ->
    R = make_ref(),
    M = erlang:monitor(process, self()),
    A = erlang:alias([reply]),
    T = erlang:send_after(100000, self(), x),
    [binary_to_term(term_to_binary(X)) =:= X || X <- [R, M, A, T]].

%% --- a reference as a map key and an ets key -----------------------------
key_uses() ->
    R = make_ref(),
    M = #{R => a},
    T = ets:new(t, [set]),
    true = ets:insert(T, {R, b}),
    {maps:get(R, M), element(2, hd(ets:lookup(T, R))),
     maps:get(binary_to_term(term_to_binary(R)), M)}.

%% --- a decoded COPY of a live alias is still a working send destination --
copy_alias_send() ->
    A = erlang:alias([explicit_unalias]),
    C = binary_to_term(term_to_binary(A)),
    _ = (C ! hi),
    {is_reference(C), C =:= A, receive X -> X end}.

%% --- demonitor/2 with [info] on a live monitor reference -----------------
demonitor_two() ->
    P = spawn(fun() -> ok end),
    M = erlang:monitor(process, P),
    D1 = erlang:demonitor(M, [info]),
    D2 = erlang:demonitor(M),
    receive {'DOWN', _, process, P, _} -> ok after 1000 -> timeout end,
    {is_boolean(D1), D2}.

%% --- DIVERGENCE: a send to a bare make_ref/0 ----------------------------
send_bare_ref() ->
    try make_ref() ! x catch C:E -> {C, E} end.

%% --- DIVERGENCE: a send to a plain monitor reference --------------------
send_monitor_ref() ->
    P = spawn(fun() -> ok end),
    M = erlang:monitor(process, P),
    try M ! x catch Cl:E -> {Cl, E} end.

%% --- DIVERGENCE: a send to a timer reference ---------------------------
send_timer_ref() ->
    T = erlang:send_after(100000, self(), x),
    try T ! y catch C:E -> {C, E} end.

%% --- DIVERGENCE: {alias, _} on a self-monitor --------------------------
mon_self_alias() ->
    R = erlang:monitor(process, self(), [{alias, reply_demonitor}]),
    {is_reference(R), erlang:unalias(R)}.

%% --- DIVERGENCE: unalias/1 of a decoded copy ---------------------------
unalias_copy() ->
    A = erlang:alias([explicit_unalias]),
    C = binary_to_term(term_to_binary(A)),
    try erlang:unalias(C) catch Cl:E -> {Cl, E} end.

%% --- DIVERGENCE: cancel_timer/1 of a decoded copy ----------------------
cancel_timer_copy() ->
    T = erlang:send_after(100000, self(), x),
    C = binary_to_term(term_to_binary(T)),
    try erlang:cancel_timer(C) of V -> is_integer(V) catch Cl:E -> {Cl, E} end.
"#;

/// The fixture module as `(module, source)` — one scratch directory for both runtimes.
const FIXTURES: [(&str, &str); 1] = [("refids", REFIDS_SRC)];

// ---------------------------------------------------------------------------------------
// The agreeing cases (`beam_rt run` vs the live oracle's `-eval`)
// ---------------------------------------------------------------------------------------

/// Every case the two runtimes must print identically: the label, the fixture entry point and the
/// exact line both runtimes print for it. Two independent mint routes appear in most entries, so a
/// regression in either id space is caught.
const AGREEMENTS: [(&str, &str, &str); 9] = [
    (
        "make_ref/0 through is_reference, =:=, binary_to_term, demonitor and unalias",
        "make_ref_consumers",
        "{true,true,true,true,false}",
    ),
    (
        "monitor/2 and the Ref inside the delivered DOWN",
        "monitor_consumers",
        "{true,true,{true,true,normal},true}",
    ),
    (
        "alias/0 and alias/1 for every option form, through `!` and erlang:send/2",
        "alias_consumers",
        "{true,hi,hi,true,true,hi,hi,true,true,hi,hi,false,true,hi,hi,true}",
    ),
    (
        "monitor/3 {alias, reply_demonitor} + the compiled Alias ! Reply",
        "mon_alias_reply",
        "{true,replied,false}",
    ),
    (
        "send_after/3 and start_timer/3 references through is_reference and cancel_timer",
        "timer_consumers",
        "{true,true,false,true,true,true}",
    ),
    (
        "the term_to_binary/binary_to_term round trip for every mint route",
        "round_trip",
        "[true,true,true,true]",
    ),
    (
        "a reference as a map key and an ets key",
        "key_uses",
        "{a,b,a}",
    ),
    (
        "a decoded COPY of a live alias is still a working send destination",
        "copy_alias_send",
        "{true,true,hi}",
    ),
    (
        "demonitor/2 with [info] on a live monitor reference",
        "demonitor_two",
        "{true,true}",
    ),
];

// ---------------------------------------------------------------------------------------
// The known divergences (both observed values pinned, never asserted away)
// ---------------------------------------------------------------------------------------

/// Each entry: the label, the fixture entry point, this runtime's current answer and the live
/// oracle's answer for the same workload.
const DIVERGENCES: [(&str, &str, &str, &str); 6] = [
    (
        "a send to a bare make_ref/0: typed badarg here, a silent drop there",
        "send_bare_ref",
        "{error,badarg}",
        "x",
    ),
    (
        "a send to a plain monitor reference: typed badarg here, a silent drop there",
        "send_monitor_ref",
        "{error,badarg}",
        "x",
    ),
    (
        "a send to a timer reference: typed badarg here, a silent drop there",
        "send_timer_ref",
        "{error,badarg}",
        "y",
    ),
    (
        "{alias, _} on a self-monitor: an alias is registered here, the C clears it",
        "mon_self_alias",
        "{true,true}",
        "{true,false}",
    ),
    (
        "unalias/1 of a decoded copy: the term-keyed registry misses it here",
        "unalias_copy",
        "false",
        "true",
    ),
    (
        "cancel_timer/1 of a decoded copy: the term-keyed registry misses it here",
        "cancel_timer_copy",
        "{error,badarg}",
        "true",
    ),
];

// ---------------------------------------------------------------------------------------
// The `-eval` route (the evaluator's own `!` operator and BIF dispatch)
// ---------------------------------------------------------------------------------------

/// The `-eval` program both runtimes are given for a value expression: print it with `~w`, then halt.
fn eval_program(value: &str) -> String {
    format!("io:format(\"~w~n\",[{value}]), halt(0).")
}

/// The two send entry points on the `-eval` route (`erlang:'!'/2` via the `!` operator and the
/// `erlang:send/2` BIF) against an **alias** destination, given as `(label, value, printed line)`.
const EVAL_AGREEMENTS: [(&str, &str, &str); 2] = [
    (
        "the `!` operator to a live alias",
        "begin A = erlang:alias([reply]), (A ! hi), receive X -> X end end",
        "hi",
    ),
    (
        "erlang:send/2 to a live alias of the explicit_unalias form",
        "begin A = erlang:alias([explicit_unalias]), erlang:send(A, yo), receive X -> X end end",
        "yo",
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

/// Writes and compiles the fixture module into one scratch directory, or `None` with a loud skip when
/// the toolchain cannot do it.
fn build_fixtures(erlc: &Path) -> Option<Fixtures> {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    // `$TMPDIR` (via `std::env::temp_dir`); the dir is handed to the oracle as a single `-pa`, so it
    // must not contain a `lists.beam` (this crate's `tests/fixtures/` does; this one does not).
    let dir = std::env::temp_dir().join(format!("beam_rt_refids_{}_{n}", std::process::id()));
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
    let output = match Command::new(erlc).arg("-o").arg(&dir).args(&names).current_dir(&dir).output()
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

/// The live oracle and the compiled fixtures, or `None` after a loud skip. Every test needs both.
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

/// Runs `program args` through the shared bounded runner (polled at [`support::PROBE_TIMEOUT`],
/// stdin at `/dev/null`), failing loudly when the probe could not be run to completion.
fn run_bounded(program: &Path, args: &[&str]) -> Run {
    let output = support::run_bounded(program, args).unwrap_or_else(|| {
        panic!("`{}` {args:?} could not be run to completion", program.display())
    });
    Run {
        code: output.status.code().unwrap_or(-1),
        out: String::from_utf8_lossy(&output.stdout).trim().to_string(),
        err: String::from_utf8_lossy(&output.stderr).trim().to_string(),
    }
}

/// `beam_rt -pa DIR run DIR/refids.beam refids:<function>/0` — the scheduled-process route, which
/// prints the result as `~w~n`.
fn run_scheduled(fixtures: &Fixtures, function: &str) -> Run {
    let beam = fixtures.beam("refids");
    let beam = beam
        .to_str()
        .unwrap_or_else(|| panic!("the fixture path is not UTF-8: {}", beam.display()));
    let spec = format!("refids:{function}/0");
    run_bounded(
        Path::new(BINARY),
        &["-pa", fixtures.dir_str(), "run", beam, &spec],
    )
}

/// `beam_rt -noshell -eval <expr>` through the built binary.
fn beam_rt_eval(expr: &str) -> Run {
    run_bounded(Path::new(BINARY), &["-noshell", "-eval", expr])
}

/// The live oracle's line for `refids:<function>/0`, in the shape the `run` action prints.
fn oracle_line(oracle: &Path, fixtures: &Fixtures, function: &str) -> Option<String> {
    support::oracle_eval(
        oracle,
        Some(&fixtures.dir),
        &eval_program(&format!("refids:{function}()")),
    )
}

// ---------------------------------------------------------------------------------------
// The tests
// ---------------------------------------------------------------------------------------

/// Every public mint route × consumer path that the oracle can answer prints exactly what the live
/// oracle prints for the same function.
///
/// The expected line is asserted for this runtime **and** re-probed from the live oracle, so a stale
/// recording cannot pass: a change to either the runtime or the oracle fails loudly.
#[test]
fn the_reference_consumer_matrix_matches_the_live_oracle() {
    let Some((oracle, fixtures)) = fixtures_or_skip() else {
        return;
    };
    for (label, function, expected) in AGREEMENTS {
        let ours = run_scheduled(&fixtures, function);
        assert_eq!(
            ours.code, 0,
            "{label}: `beam_rt run refids:{function}/0` must exit 0, got {}: {}",
            ours.code, ours.err
        );
        assert!(
            !ours.err.contains("unsupported"),
            "{label}: a reference consumer path is not a capability gap: {}",
            ours.err
        );
        assert!(ours.err.is_empty(), "{label}: stderr {}", ours.err);
        assert_eq!(ours.out, expected, "{label}: beam_rt's answer");

        let theirs = oracle_line(&oracle, &fixtures, function)
            .unwrap_or_else(|| panic!("the live oracle could not answer refids:{function}/0"));
        assert_eq!(
            theirs, expected,
            "{label}: the recorded value must be the live oracle's answer"
        );
        assert_eq!(ours.out, theirs, "{label}: the two runtimes must agree");
    }
}

/// The two send entry points on the `-eval` route (the evaluator's own `!` operator and the
/// `erlang:send/2` BIF) resolve a live alias destination identically to the oracle.
#[test]
fn the_eval_route_send_paths_match_the_live_oracle() {
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
        assert_eq!(ours.out, theirs, "{label}: the two runtimes must agree");
    }
}

/// The known **divergences**, reported with **both** observed values rather than asserted away. Each
/// is a real difference from the C/oracle that the reference-id design deliberately leaves in place
/// (see the module docs); pinning both sides keeps it visible and makes any future change to either
/// side fail loudly and force this list to be revisited.
///
/// The one this fix is *about* — a bare reference whose id **collides** with a live alias — is not
/// here: forcing the collision needs the runtime's own internals (the unit suite's
/// `resolve_send_target_rejects_a_bare_reference_that_collides_with_a_live_alias`), and the oracle's
/// single id space can never produce such a pair.
#[test]
fn the_known_divergences_are_reported_with_both_observed_values() {
    let Some((oracle, fixtures)) = fixtures_or_skip() else {
        return;
    };
    for (label, function, our_expected, oracle_expected) in DIVERGENCES {
        let ours = run_scheduled(&fixtures, function);
        assert_eq!(
            ours.code, 0,
            "{label}: the probe catches its own failure, so it must exit 0, got {}: {}",
            ours.code, ours.err
        );
        assert_eq!(
            ours.out, our_expected,
            "{label}: beam_rt's current (divergent) answer"
        );

        let theirs = oracle_line(&oracle, &fixtures, function)
            .unwrap_or_else(|| panic!("the live oracle could not answer refids:{function}/0"));
        assert_eq!(theirs, oracle_expected, "{label}: the oracle's answer");
        assert_ne!(
            ours.out, theirs,
            "{label}: no longer divergent — move this case to the agreement list"
        );
    }
}
