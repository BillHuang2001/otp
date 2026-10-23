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
//! **Applying a decoded `NEW_FUN_EXT` closure**, end to end on the **default** execution tier and
//! on `BEAM_RT_TIER=interpreter` (contract A.5/A.2's fun-image surface).
//!
//! # The three raise classes
//!
//! A `NEW_FUN_EXT` image carries the fun entry's identity as `(module, 16-byte uniq, index,
//! old_index, old_uniq)`; the decoder rebuilds the entry from those fields and the failure
//! surfaces only when the fun is **applied** (`erts/emulator/beam/beam_common.c:1696-1722`):
//!
//! 1. **A genuine descriptor** — the image names a lambda the defining module really carries (its
//!    `index`/`old_uniq` pair is a `FunT` entry and its 16-byte `uniq` equals the module's
//!    checksum) — applies normally: the fixture answers `{applied,42}` for `fun(X) -> X + 1 end`
//!    applied to `41`.
//! 2. **An identity that names no lambda** — an `index` no `FunT` entry carries (`999999`,
//!    `4294967295`) or a corrupted 16-byte `uniq` — is the C's `EXC_BADFUN`, whose
//!    `expand_error_value` renders `TUPLE2(am_badfun, fvalue)` (`:750-772`): a `catch` observes
//!    `{error, {badfun, F}}` with `F` the very fun that was applied (`F =:= G`).
//! 3. **A module atom with no loadable code** — the C hands it to
//!    `error_handler:undefined_lambda/3` (`lib/kernel/src/error_handler.erl`), whose crash path
//!    raises `error:undef` with a synthesized **innermost stacktrace item** `{Fun, ArgList, []}`
//!    (the applied fun, the real argument list, the empty location). The *reason* is the bare
//!    atom `undef`; the 3-tuple is the trace's head item, and it is never a real callee frame.
//!
//! All three routes go through the same engine seam (`families::funs::call_fun` →
//! `ExecutionContext::resolve_local_fun` → `RuntimeContext::imported_fun_target`): a **direct**
//! call `G(A)`, `lists:map/2` and `erlang:apply/2` are measured separately, which is why the
//! report also carries the head of the stacktrace (the `{badfun, F}` classes keep the real
//! caller frame — `{nfbadfun,apply_route,3}` for a direct/`apply` route, `{lists,map,2}` for the
//! `map` one).
//!
//! # What the fixture reports, and why it renders no fun
//!
//! The fixture does the whole round trip in Erlang — build a closure, `term_to_binary/1` it,
//! patch one identity field with `binary:part/2,3` + a binary construction, `binary_to_term/1`
//! the result, apply it inside `try … catch` — and reports atoms, booleans and small integers
//! only: this runtime has no `#Fun<…>` printer, so the assertions are the term's *structure*
//! (`F =:= G` for the reason's fun identity, `{applied_fun, Args, Loc, is_function(F)}` for the
//! trace item). The `NEW_FUN_EXT` offsets it patches are the wire layout: tag(0), size(1..4),
//! arity(5), `uniq`(6..21), `index`(22..25), `num_free`(26..29), module(30..).
//!
//! `self_index/0` rewrites the image's **own** index back into the image and must still apply:
//! that is the control which proves the patched offset really is the `index` field (a wrong
//! offset would turn the control into a decode or `badfun` failure), so `badfun_*`'s answer
//! cannot be an artefact of the injection.
//!
//! Every expected string below was **measured** on the system oracle (OTP 29.1.1 / `erts-17.1`)
//! and is re-checked against it on every run; the two tiers must reproduce it exactly.
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

mod support;

/// The banner for the loud skip.
const BANNER: &str = "decoded fun badfun";

/// The executable under test.
const BINARY: &str = env!("CARGO_BIN_EXE_beam_rt");

/// The fixture: one `fun(X) -> X + 1 end` image doctored in exactly one identity field at a
/// time, applied on three routes, reporting the observable class/reason/trace-head.
///
/// Exported probes (all arity 0 — the CLI runs a `module:function/0` spec):
///
/// * `control_direct/0`, `control_map/0`, `control_apply/0` — the **control**: the unpatched
///   image applies on each route (`{applied,42}`);
/// * `self_index/0` — the image's own `index` written back into the image, which must still
///   apply (the offset self-check);
/// * `badfun_direct/0`, `badfun_map/0`, `badfun_apply/0` — `index = 999999` on each route;
/// * `big_badfun/0` — `index = 4294967295` (the whole 32-bit field);
/// * `uniq_badfun/0` — the first of the 16 `uniq` bytes flipped (a stale build of the module);
/// * `undef_direct/0`, `undef_map/0`, `undef_apply/0` — the module atom replaced by the
///   same-length `nofunxyz`, which no module file provides;
/// * `trace_direct/0`, `trace_map/0`, `trace_apply/0` — the same `undef` image reporting the
///   **module of every** stacktrace item, for the "no fabricated/decoration frames" check.
const FIXTURE_SRC: &str = r#"
-module(nfbadfun).
-export([control_direct/0, control_map/0, control_apply/0,
         self_index/0,
         badfun_direct/0, badfun_map/0, badfun_apply/0,
         big_badfun/0, uniq_badfun/0,
         undef_direct/0, undef_map/0, undef_apply/0,
         trace_direct/0, trace_map/0, trace_apply/0]).

%% A `NEW_FUN_EXT` image of this module's own `fun(X) -> X + 1 end`:
%%   tag(0) size(1..4) arity(5) uniq(6..21) index(22..25) num_free(26..29) module(30..)
image() ->
    F = fun(X) -> X + 1 end,
    term_to_binary(F).

index_of(Bin) ->
    <<Idx:32>> = binary:part(Bin, 23, 4),
    Idx.

with_index(Bin, Idx) ->
    <<Head:23/binary, _:32, Tail/binary>> = Bin,
    <<Head/binary, Idx:32, Tail/binary>>.

flip_uniq(Bin) ->
    <<Head:7/binary, B:8, Tail/binary>> = Bin,
    <<Head/binary, (B bxor 1):8, Tail/binary>>.

%% The module atom is SMALL_ATOM_UTF8 (119) + a one-byte length at 0-based 32 + the name from
%% 0-based 33; the replacement must be the same length so every following field keeps its offset.
with_module(Bin, Name) ->
    <<Len:8>> = binary:part(Bin, 32, 1),
    byte_size(Name) =:= Len orelse erlang:error({module_name_length, Name, Len}),
    <<Head:33/binary, _:Len/binary, Tail/binary>> = Bin,
    <<Head/binary, Name/binary, Tail/binary>>.

apply_route(direct, G, A) -> G(A);
apply_route(map, G, A) -> hd(lists:map(G, [A]));
apply_route(apply, G, A) -> erlang:apply(G, [A]).

head({badfun, _}) -> badfun;
head(R) -> R.

matches({badfun, F}, G) -> F =:= G;
matches(_, _) -> false.

frame_head([{M, F, A, _} | _]) -> {M, F, A};
frame_head([{Fun, Args, Loc} | _]) when is_function(Fun) ->
    {applied_fun, Args, Loc, is_function(Fun)};
frame_head([]) -> none.

%% Whether the item right after the trace's head is the `lists:map/2` the map route applied the
%% fun from (`false` when the head is all there is, or the next item is something else).
after_head([_, {M, F, A, _} | _]) -> {M, F, A} =:= {lists, map, 2};
after_head(_) -> false.

modules([{M, _, _, _} | Rest]) -> [M | modules(Rest)];
modules([{Fun, _, _} | Rest]) when is_function(Fun) -> [applied_fun | modules(Rest)];
modules([]) -> [].

control(Route) ->
    G = binary_to_term(image()),
    {applied, apply_route(Route, G, 41)}.

control_bin(Route, Bin) ->
    G = binary_to_term(Bin),
    {applied, apply_route(Route, G, 41)}.

self_index() ->
    Bin = image(),
    control_bin(direct, with_index(Bin, index_of(Bin))).

badfun(Route, Kind) ->
    G = binary_to_term(bad_index(Kind)),
    try apply_route(Route, G, 41) of
        V -> {applied, V}
    catch C:R:S -> {C, head(R), matches(R, G), frame_head(S)}
    end.

bad_index(999999) -> with_index(image(), 999999);
bad_index(4294967295) -> with_index(image(), 4294967295);
bad_index(uniq) -> flip_uniq(image()).

undef(Route) ->
    G = binary_to_term(with_module(image(), <<"nofunxyz">>)),
    try apply_route(Route, G, 41) of
        V -> {applied, V}
    catch C:R:S -> {C, R, frame_head(S), after_head(S)}
    end.

trace(Route) ->
    G = binary_to_term(with_module(image(), <<"nofunxyz">>)),
    try apply_route(Route, G, 41) of
        V -> {applied, V}
    catch C:R:S -> {C, R, modules(S)}
    end.

control_direct() -> control(direct).
control_map() -> control(map).
control_apply() -> control(apply).

badfun_direct() -> badfun(direct, 999999).
badfun_map() -> badfun(map, 999999).
badfun_apply() -> badfun(apply, 999999).
big_badfun() -> badfun(direct, 4294967295).
uniq_badfun() -> badfun(direct, uniq).

undef_direct() -> undef(direct).
undef_map() -> undef(map).
undef_apply() -> undef(apply).

trace_direct() -> trace(direct).
trace_map() -> trace(map).
trace_apply() -> trace(apply).
"#;

/// Each probe and the answer the oracle prints for it (`erl -noshell -pa <dir> -eval
/// 'io:format("~w~n",[nfbadfun:<probe>()]).'`, OTP 29.1.1 / `erts-17.1`).
///
/// The three groups are the raise classes: `{applied,42}` = a genuine descriptor (including
/// `self_index`), `{error,badfun,true,…}` = no lambda under that identity (`true` is `F =:= G`,
/// the reason's fun being the applied one), `{error,undef,…}` = a module with no loadable code,
/// whose 3rd element is the trace's head item `{applied_fun, ArgList, Loc, is_function}` (`Args`
/// is the real `[41]`) and whose 4th is whether the next item is `{lists,map,2}`.
const PROBES: [(&str, &str); 12] = [
    ("control_direct", "{applied,42}"),
    ("control_map", "{applied,42}"),
    ("control_apply", "{applied,42}"),
    ("self_index", "{applied,42}"),
    ("badfun_direct", "{error,badfun,true,{nfbadfun,apply_route,3}}"),
    ("badfun_map", "{error,badfun,true,{lists,map,2}}"),
    ("badfun_apply", "{error,badfun,true,{nfbadfun,apply_route,3}}"),
    ("big_badfun", "{error,badfun,true,{nfbadfun,apply_route,3}}"),
    ("uniq_badfun", "{error,badfun,true,{nfbadfun,apply_route,3}}"),
    ("undef_direct", "{error,undef,{applied_fun,[41],[],true},false}"),
    ("undef_map", "{error,undef,{applied_fun,[41],[],true},true}"),
    ("undef_apply", "{error,undef,{applied_fun,[41],[],true},false}"),
];

/// The `undef` images' full frame-module lists, for the trace-shape check: the probe and whether
/// `lists` (the real callee of the `map` route) is part of the trace.
const TRACES: [(&str, bool); 3] = [
    ("trace_direct", false),
    ("trace_map", true),
    ("trace_apply", false),
];

/// The execution tiers every probe runs on: the process **default** (no `BEAM_RT_TIER`) and the
/// `BEAM_RT_TIER=interpreter` reference tier.
const TIERS: [(Option<&str>, &str); 2] = [(None, "default"), (Some("interpreter"), "interpreter")];

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

/// The compiled fixture in one scratch directory (removed on drop).
struct Fixtures {
    /// The scratch directory; a safe single `-pa` for both runtimes (no `lists.beam`).
    dir: PathBuf,
}

impl Fixtures {
    /// The compiled `nfbadfun.beam`.
    fn beam(&self) -> PathBuf {
        self.dir.join("nfbadfun.beam")
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
        if let Err(error) = std::fs::remove_dir_all(&self.dir) {
            eprintln!(
                "warning: could not remove the fixture dir {}: {error}",
                self.dir.display()
            );
        }
    }
}

/// Writes and compiles the fixture, or reports the loud skip.
fn build_fixtures(erlc: &Path) -> Option<Fixtures> {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("beam_rt_nfbadfun_{}_{n}", std::process::id()));
    if let Err(error) = std::fs::create_dir_all(&dir) {
        support::skip_loudly(BANNER, &format!("cannot prepare {}: {error}", dir.display()));
        return None;
    }
    if let Err(error) = std::fs::write(dir.join("nfbadfun.erl"), FIXTURE_SRC) {
        support::skip_loudly(BANNER, &format!("cannot write nfbadfun.erl: {error}"));
        return None;
    }
    let output = match Command::new(erlc)
        .arg("-o")
        .arg(&dir)
        .arg("nfbadfun.erl")
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
    if !dir.join("nfbadfun.beam").is_file() {
        support::skip_loudly(BANNER, "`erlc` produced no nfbadfun.beam");
        return None;
    }
    Some(Fixtures { dir })
}

/// One finished probe: its exit code and trimmed stdout/stderr.
struct Run {
    code: i32,
    out: String,
    err: String,
}

/// Runs `program args` with `/dev/null` as stdin, polling so a probe that blocks is killed at
/// [`support::PROBE_TIMEOUT`] instead of hanging the suite.
fn run_bounded(program: &Path, args: &[&str], env: Option<(&str, &str)>) -> Run {
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some((key, value)) = env {
        command.env(key, value);
    }
    let mut child = command
        .spawn()
        .unwrap_or_else(|error| panic!("cannot spawn `{}`: {error}", program.display()));
    let deadline = Instant::now() + support::PROBE_TIMEOUT;
    let output: Output = loop {
        match child.try_wait() {
            Ok(Some(_)) => break child
                .wait_with_output()
                .unwrap_or_else(|error| panic!("cannot collect the probe's output: {error}")),
            Ok(None) => {
                assert!(
                    Instant::now() < deadline,
                    "`{}` outlived the {}s probe bound",
                    program.display(),
                    support::PROBE_TIMEOUT.as_secs()
                );
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("cannot poll `{}`: {error}", program.display()),
        }
    };
    Run {
        code: output.status.code().unwrap_or(-1),
        out: String::from_utf8_lossy(&output.stdout).trim().to_string(),
        err: String::from_utf8_lossy(&output.stderr).trim().to_string(),
    }
}

/// `beam_rt run nfbadfun.beam nfbadfun:<function>/0` on the tier `tier` names (`None` = the
/// process default), printing the result with `~w` exactly as the CLI does.
fn run_on_tier(fixtures: &Fixtures, function: &str, tier: Option<&str>) -> Run {
    let beam = fixtures.beam();
    let beam = beam
        .to_str()
        .unwrap_or_else(|| panic!("the fixture path is not UTF-8: {}", beam.display()));
    let spec = format!("nfbadfun:{function}/0");
    run_bounded(
        Path::new(BINARY),
        &["-noshell", "-pa", fixtures.dir_str(), "run", beam, &spec],
        tier.map(|tier| ("BEAM_RT_TIER", tier)),
    )
}

/// The same probe under the system `erl` — the spec both tiers are compared against.
///
/// [`support::oracle_eval`] supplies the `-pa` **before** `-eval` (erl evaluates `-eval` in
/// command-line order) and appends the `-s init stop` terminator, so the node shuts down instead
/// of waiting for ever.
fn run_oracle(oracle: &Path, fixtures: &Fixtures, function: &str) -> String {
    let expr = format!("io:format(\"~w~n\",[nfbadfun:{function}()]).");
    support::oracle_eval(oracle, Some(&fixtures.dir), &expr).unwrap_or_else(|| {
        panic!("the oracle could not answer nfbadfun:{function}/0 (see the probe diagnostic)")
    })
}

/// **A decoded `NEW_FUN_EXT` closure answers the oracle's three classes on every route, on both
/// execution tiers.**
///
/// The oracle is probed first and must print the recorded answer for all 12 probes — that is
/// what pins them to the system ERTS rather than to this suite's memory, and it is what fails if
/// the wire layout the fixture patches ever shifts. Then each tier runs the same probes and must
/// reproduce every answer **exactly** (the fixture reports atoms, booleans and small integers
/// only, so a `#Fun<…>` render is never needed).
///
/// The three classes the answers pin:
///
/// * a genuine descriptor applies (`{applied,42}`, all three routes, plus `self_index`'s
///   write-back of the image's own `index`), so neither the injection nor the route changes the
///   success path;
/// * an `index` no lambda carries (`999999`, `4294967295`) or a flipped `uniq` byte is
///   `error:{badfun, F}` with `F =:= G` — the C's `EXC_BADFUN` + `expand_error_value` — on the
///   direct call, through `lists:map/2` (whose frame is then the trace head) and through
///   `erlang:apply/2`;
/// * a module atom with no loadable code is `error:undef` whose reason is the bare atom, with the
///   applied fun as the trace's **innermost** item (`{Fun, [41], []}`): `42`-shaped `Args` and an
///   empty `Loc`, `is_function/1` true, never a real callee.
///
/// Skips **loudly** without `erl`/`erlc`.
#[test]
fn a_decoded_closure_answers_the_oracles_three_classes_on_both_tiers() {
    let Some(oracle) = support::require_oracle(BANNER) else {
        return;
    };
    let Some(erlc) = resolve_erlc() else {
        support::skip_loudly(BANNER, "no `erlc` available (and none beside $ERTS_ORACLE)");
        return;
    };
    let Some(fixtures) = build_fixtures(&erlc) else {
        return;
    };

    // The oracle is the spec.
    for (function, expected) in PROBES {
        assert_eq!(
            run_oracle(&oracle, &fixtures, function),
            expected,
            "nfbadfun:{function}/0 under the oracle"
        );
    }

    for (tier, label) in TIERS {
        for (function, expected) in PROBES {
            let run = run_on_tier(&fixtures, function, tier);
            // A **caught** raise is reported by the fixture, so the process returns normally and
            // the CLI exits 0 whatever the class was.
            assert_eq!(
                run.code, 0,
                "{label}: nfbadfun:{function}/0 must return normally; stdout={:?} stderr={:?}",
                run.out, run.err
            );
            assert!(
                !run.out.contains("unsupported"),
                "{label}: nfbadfun:{function}/0 must not be a capability gap; got {:?} \
                 stderr={:?}",
                run.out,
                run.err
            );
            assert_eq!(
                run.out, expected,
                "{label}: nfbadfun:{function}/0 must answer the oracle's {expected:?}"
            );
        }
    }
}

/// **The `undef` raise's trace names the applied fun and nothing fabricated.**
///
/// For an image whose module atom names no loadable module the engine synthesizes the innermost
/// stacktrace item `{Fun, Args, []}` itself (the oracle's `error_handler:undefined_lambda/3`
/// would, but its trailing `erl_eval`/`init` frames belong to the `-eval` driver and must not be
/// imitated). The probe reports the module of every item, so the assertions are:
///
/// * the trace starts with the applied fun (`[applied_fun, …]`) — never a real callee frame such
///   as `lists:map/2`, which appears only *after* it on the map route (`[applied_fun, lists, …]`;
///   measured on the oracle as `[{applied_fun,[41],[]}, {lists,map,2}, …]`);
/// * no item names `erl_eval`, `init` or `error_handler`, i.e. nothing is fabricated to mimic the
///   oracle's own driver frames.
///
/// Both tiers must agree. Skips **loudly** without `erl`/`erlc`.
#[test]
fn the_undefined_lambda_trace_names_the_applied_fun_and_nothing_fabricated() {
    let Some(_oracle) = support::require_oracle(BANNER) else {
        return;
    };
    let Some(erlc) = resolve_erlc() else {
        support::skip_loudly(BANNER, "no `erlc` available (and none beside $ERTS_ORACLE)");
        return;
    };
    let Some(fixtures) = build_fixtures(&erlc) else {
        return;
    };

    for (tier, label) in TIERS {
        for (function, names_lists) in TRACES {
            let run = run_on_tier(&fixtures, function, tier);
            assert_eq!(
                run.code, 0,
                "{label}: nfbadfun:{function}/0 must return normally; stdout={:?} stderr={:?}",
                run.out, run.err
            );
            assert!(
                run.out.starts_with("{error,undef,[applied_fun"),
                "{label}: {function} must raise `error:undef` with the applied fun as the \
                 innermost trace item; got {:?}",
                run.out
            );
            for fabricated in ["erl_eval", "init", "error_handler"] {
                assert!(
                    !run.out.contains(fabricated),
                    "{label}: {function}'s trace must not name `{fabricated}`; got {:?}",
                    run.out
                );
            }
            assert_eq!(
                run.out.contains("lists"),
                names_lists,
                "{label}: {function}: only the map route's trace carries the real `lists` callee; \
                 got {:?}",
                run.out
            );
        }
    }
}
