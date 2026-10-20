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

//! The **nested fun-application matrix** for the fun-applying registry class.
//!
//! [`super::super::apply::prefers_module_code_for_fun_apply`] makes
//! [`RuntimeContext::resolve_external_call`](crate::context::RuntimeContext::resolve_external_call)
//! decline the host fast path for a `beam_bifs::FUN_CALLER_BIFS` key **when the context cannot
//! pump** (`may_pump_scheduler() == false`, i.e. inside a scheduler slice). The callee's own
//! Erlang code then runs in the caller's machine, so the compiler's `call_fun` applies the
//! caller's fun **in that machine**: a blocking `receive` inside the fun suspends the *process*
//! (`RunOutcome::Wait` → runner park → resume-on-send) instead of folding into the typed
//! `beam_engine::blocked()` gap the host's nested run would report.
//!
//! This module is the evidence matrix for that seam: four fun-applying `lists` keys × four fun
//! kinds, every cell **executed as a scheduled process** (`Runtime::run_scheduled`, the route
//! whose context is `RuntimeContext::for_process`) and compared against the live system oracle's
//! own answer for the same compiled fixture.
//!
//! | fun kind | what the fun does | why it is a cell |
//! |---|---|---|
//! | **pure** | `X * 2` | the control: no blocking at all |
//! | **`io:format`** | writes through the io device floor to the process's group leader (stdout) | the floor's own round trip |
//! | **file read** | `file:read_file/1` through the file-server floor | a real `erlang:monitor/2` + `!` + `receive` request reply in `file:file_request/2` |
//! | **`receive`** | `receive {mx_value, V} -> …` for a message a **spawned child** sends | only another *process* can satisfy it — the suspension itself |
//!
//! Every fixture function returns `{Result, Observed}`: `Result` is what the key answered and
//! `Observed` is the ledger the fun itself wrote into the process dictionary on every
//! application, so the answer pins **how many times** (and with which arguments) the fun really
//! ran — a fun that was never applied, applied once instead of three times, or applied twice by
//! a resumed-and-restarted apply is a different term.
//!
//! # The three additional pins
//!
//! * [`the_host_fast_path_still_answers_for_a_context_that_may_pump`] runs the **pure** and
//!   **`io:format`** cells again through the `-eval` route ([`RuntimeContext::with_output`],
//!   `may_pump_scheduler() == true`), where the seam keeps the host fast path: the same
//!   oracle-identical answer must come out, and the io cell's text — captured from the caller's
//!   own sink, which the host's nested run inherits — must be the oracle's own lines, so the two
//!   routes cannot silently swap behaviour.
//! * [`maps_and_ets_fun_keys_also_suspend_a_scheduled_process`] covers the class beyond `lists`:
//!   `maps:map/2` and `ets:foldl/3` with a blocking fun, oracle-compared.
//! * [`a_nested_apply_that_bypasses_the_seam_is_still_the_typed_gap`] is the **negative**
//!   control: the same workload applied through `erlang:apply/3` (which reaches the registry
//!   directly, never the cross-module seam) is the documented residual — a typed capability gap,
//!   never a fabricated value.
//!
//! A missing `erlc`/`erl` is a **loud** skip ([`skip_loudly`]), never a silent pass. Every
//! fixture is compiled from this file's own source into a self-removing scratch directory.

use std::path::Path;

use beam_bifs::BytesSink;
use beam_term::Eterm;

use crate::context::test_support::{
    compile_fixtures, find_on_path, resolve_erlc, resolve_oracle, run_oracle_probe_exact,
    scratch_dir, skip_loudly, FixtureDir,
};
use crate::context::RuntimeContext;
use crate::runtime::Runtime;

/// The fixture module's name (the source stem and the expected `.beam`).
const FIXTURE_MODULE: &str = "fapplymx";

/// The loud-skip banner prefix for this module's tests.
const BANNER: &str = "nested fun-application matrix";

/// The fixture module.
///
/// The funs are handed to `lists:`/`maps:`/`ets:` — a **cross-module** call — so the closure is
/// genuinely materialized (`make_fun3`) and genuinely applied by the library's own code, exactly
/// the shape `super::blocking_fun_apply` documents. The path the file-reading cells read is
/// substituted into the source (`@@FILE@@`) because both runtimes must read the *same* file.
const FIXTURE_SRC: &str = r#"
-module(fapplymx).
-export([flat_pure/0, flat_io/0, flat_file/0, flat_recv/0,
         map_pure/0, map_io/0, map_file/0, map_recv/0,
         feach_pure/0, feach_io/0, feach_file/0, feach_recv/0,
         fold_pure/0, fold_io/0, fold_file/0, fold_recv/0,
         maps_recv/0, ets_recv/0, bypassing_apply/0]).

path() -> "@@FILE@@".

%% The observation ledger: every fun application records its own arguments, so the returned
%% term pins HOW MANY TIMES (and with which values) the fun really ran.
reset() -> erase(mx_seen), ok.

seen_list(undefined) -> [];
seen_list(L) -> L.

note(T) -> put(mx_seen, [T | seen_list(get(mx_seen))]), ok.

seen() -> lists:reverse(seen_list(get(mx_seen))).

%% ---- the blocking fun kind's other process: only it can satisfy the receive ----

%% ---- lists:flatmap/2 ----
flat_pure() ->
    reset(),
    R = lists:flatmap(fun(X) -> note({pure, X}), [X * 2] end, [1, 2, 3]),
    {R, seen()}.

flat_io() ->
    reset(),
    R = lists:flatmap(fun(X) ->
                          ok = io:format("mx io ~w~n", [X]),
                          note({io, X}),
                          [X * 2]
                      end, [1, 2, 3]),
    {R, seen()}.

flat_file() ->
    reset(),
    P = path(),
    R = lists:flatmap(fun(X) ->
                          {ok, Bin} = file:read_file(P),
                          N = byte_size(Bin),
                          note({file, X, N}),
                          [X + N]
                      end, [1, 2, 3]),
    {R, seen()}.

flat_recv() ->
    reset(),
    Self = self(),
    _ = spawn(fun() -> Self ! {mx_value, 42} end),
    R = lists:flatmap(fun(X) ->
                          receive {mx_value, V} -> note({recv, X, V}), [X + V] end
                      end, [1]),
    {R, seen()}.

%% ---- lists:map/2 ----
map_pure() ->
    reset(),
    R = lists:map(fun(X) -> note({pure, X}), X * 2 end, [1, 2, 3]),
    {R, seen()}.

map_io() ->
    reset(),
    R = lists:map(fun(X) ->
                      ok = io:format("mx io ~w~n", [X]),
                      note({io, X}),
                      X * 2
                  end, [1, 2, 3]),
    {R, seen()}.

map_file() ->
    reset(),
    P = path(),
    R = lists:map(fun(X) ->
                      {ok, Bin} = file:read_file(P),
                      N = byte_size(Bin),
                      note({file, X, N}),
                      X + N
                  end, [1, 2, 3]),
    {R, seen()}.

map_recv() ->
    reset(),
    Self = self(),
    _ = spawn(fun() -> Self ! {mx_value, 42} end),
    R = lists:map(fun(X) ->
                      receive {mx_value, V} -> note({recv, X, V}), X + V end
                  end, [1]),
    {R, seen()}.

%% ---- lists:foreach/2 (answers `ok`, so the ledger is the whole observation) ----
feach_pure() ->
    reset(),
    R = lists:foreach(fun(X) -> note({pure, X}) end, [1, 2, 3]),
    {R, seen()}.

feach_io() ->
    reset(),
    R = lists:foreach(fun(X) ->
                          ok = io:format("mx io ~w~n", [X]),
                          note({io, X})
                      end, [1, 2, 3]),
    {R, seen()}.

feach_file() ->
    reset(),
    P = path(),
    R = lists:foreach(fun(X) ->
                          {ok, Bin} = file:read_file(P),
                          note({file, X, byte_size(Bin)})
                      end, [1, 2, 3]),
    {R, seen()}.

feach_recv() ->
    reset(),
    Self = self(),
    _ = spawn(fun() -> Self ! {mx_value, 42} end),
    R = lists:foreach(fun(X) ->
                          receive {mx_value, V} -> note({recv, X, V}) end
                      end, [1]),
    {R, seen()}.

%% ---- lists:foldl/3 ----
fold_pure() ->
    reset(),
    R = lists:foldl(fun(X, A) -> note({pure, X}), A + X * 2 end, 0, [1, 2, 3]),
    {R, seen()}.

fold_io() ->
    reset(),
    R = lists:foldl(fun(X, A) ->
                        ok = io:format("mx io ~w~n", [X]),
                        note({io, X}),
                        A + X * 2
                    end, 0, [1, 2, 3]),
    {R, seen()}.

fold_file() ->
    reset(),
    P = path(),
    R = lists:foldl(fun(X, A) ->
                        {ok, Bin} = file:read_file(P),
                        N = byte_size(Bin),
                        note({file, X, N}),
                        A + X + N
                    end, 0, [1, 2, 3]),
    {R, seen()}.

fold_recv() ->
    reset(),
    Self = self(),
    _ = spawn(fun() -> Self ! {mx_value, 42} end),
    R = lists:foldl(fun(X, A) ->
                        receive {mx_value, V} -> note({recv, X, V}), A + X + V end
                    end, 0, [1]),
    {R, seen()}.

%% ---- the class beyond `lists`: `maps:map/2` and `ets:foldl/3` ----
maps_recv() ->
    reset(),
    Self = self(),
    _ = spawn(fun() -> Self ! {mx_value, 42} end),
    R = maps:map(fun(K, V) ->
                     receive {mx_value, M} -> note({recv, K, M}), V + M end
                 end, #{a => 1}),
    {R, seen()}.

ets_recv() ->
    reset(),
    T = ets:new(mx_tab, [ordered_set]),
    true = ets:insert(T, {1, one}),
    Self = self(),
    _ = spawn(fun() -> Self ! {mx_value, 42} end),
    R = ets:foldl(fun({K, _V}, A) ->
                      receive {mx_value, M} -> note({recv, K, M}), A + K + M end
                  end, 0, T),
    ets:delete(T),
    {R, seen()}.

%% The residual's shape: the SAME workload as `flat_recv/0`, but applied through
%% `erlang:apply/3`, which reaches the registry directly and never the cross-module seam. The
%% `{M, Fun, Args}` destructuring keeps the compiler from specializing the call into an `apply`
%% instruction with literal operands.
bypassing_apply() ->
    reset(),
    Self = self(),
    _ = spawn(fun() -> Self ! {mx_value, 42} end),
    F = fun(X) -> receive {mx_value, V} -> note({recv, X, V}), [X + V] end end,
    {M, Fun, Args} = {lists, flatmap, [F, [1]]},
    erlang:apply(M, Fun, Args).
"#;

/// The file the file-reading cells read; two bytes, so `byte_size/1` is a small unambiguous 2.
const PAYLOAD: &[u8] = b"xy";

/// One matrix cell.
struct Cell {
    /// The fun-applying registry key the cell exercises.
    key: &'static str,
    /// The fun kind the key applies.
    kind: &'static str,
    /// The fixture function that performs the cell.
    function: &'static str,
}

/// The matrix: four `lists` keys × four fun kinds.
const CELLS: &[Cell] = &[
    Cell { key: "lists:flatmap/2", kind: "pure", function: "flat_pure" },
    Cell { key: "lists:flatmap/2", kind: "io:format", function: "flat_io" },
    Cell { key: "lists:flatmap/2", kind: "file read", function: "flat_file" },
    Cell { key: "lists:flatmap/2", kind: "receive", function: "flat_recv" },
    Cell { key: "lists:map/2", kind: "pure", function: "map_pure" },
    Cell { key: "lists:map/2", kind: "io:format", function: "map_io" },
    Cell { key: "lists:map/2", kind: "file read", function: "map_file" },
    Cell { key: "lists:map/2", kind: "receive", function: "map_recv" },
    Cell { key: "lists:foreach/2", kind: "pure", function: "feach_pure" },
    Cell { key: "lists:foreach/2", kind: "io:format", function: "feach_io" },
    Cell { key: "lists:foreach/2", kind: "file read", function: "feach_file" },
    Cell { key: "lists:foreach/2", kind: "receive", function: "feach_recv" },
    Cell { key: "lists:foldl/3", kind: "pure", function: "fold_pure" },
    Cell { key: "lists:foldl/3", kind: "io:format", function: "fold_io" },
    Cell { key: "lists:foldl/3", kind: "file read", function: "fold_file" },
    Cell { key: "lists:foldl/3", kind: "receive", function: "fold_recv" },
];

/// The compiled fixture: the module's own scratch directory plus the payload file's.
struct Fixture {
    /// Holds `fapplymx.beam`.
    dir: FixtureDir,
    /// Holds the payload the file-reading cells read (kept alive for the whole test).
    _payload: FixtureDir,
}

impl Fixture {
    /// The compiled `.beam`.
    fn beam(&self) -> std::path::PathBuf {
        self.dir.join("fapplymx.beam")
    }
}

/// Resolves the toolchain the matrix needs — `erlc` for the fixture and `erl` for the oracle —
/// or reports a **loud** skip and answers `None`.
fn toolchain() -> Option<(std::path::PathBuf, std::path::PathBuf)> {
    // `run_scheduled` boots the runtime on demand, which resolves the system code path
    // (natively from the filesystem, else `$ERTS_STDLIB_DIR` or the system `erl`).
    if std::env::var_os("ERTS_STDLIB_DIR").is_none() && find_on_path("erl").is_none() {
        skip_loudly(BANNER, "no `erl` on $PATH and no $ERTS_STDLIB_DIR");
        return None;
    }
    let Some(erlc) = resolve_erlc() else {
        skip_loudly(BANNER, "no `erlc` (and none beside $ERTS_ORACLE either)");
        return None;
    };
    let Some(oracle) = resolve_oracle() else {
        skip_loudly(BANNER, "no `erl` oracle (and none beside $ERTS_ORACLE either)");
        return None;
    };
    Some((erlc, oracle))
}

/// Compiles the fixture and writes the payload file the file-reading cells read.
///
/// The payload path is substituted into the source as an Erlang string literal, so both runtimes
/// read the very same file; the escapes are applied defensively (a POSIX path never needs them).
fn fixture(erlc: &Path) -> Fixture {
    let payload = scratch_dir("fun_apply_matrix_data");
    let file = payload.join("mx_payload.txt");
    std::fs::write(&file, PAYLOAD).expect("the payload file is writable");
    let literal = file
        .to_str()
        .unwrap_or_else(|| panic!("the payload path is not UTF-8: {}", file.display()))
        .replace('\\', "\\\\")
        .replace('"', "\\\"");
    let source = FIXTURE_SRC.replace("@@FILE@@", &literal);
    let dir = compile_fixtures(erlc, "fun_apply_matrix", &[(FIXTURE_MODULE, &source)]);
    Fixture {
        dir,
        _payload: payload,
    }
}

/// The oracle's own answer for one cell: every stdout line the system `erl` printed for
/// `fapplymx:<function>()`, the last of which is the `~w` render of the returned term (an
/// `io:format` cell prints its fun's lines first).
fn oracle_lines(oracle: &Path, fixture: &Fixture, function: &str) -> Vec<String> {
    let eval = format!("io:format(\"~w~n\", [{FIXTURE_MODULE}:{function}()])");
    let lines = run_oracle_probe_exact(oracle, fixture.dir.path(), &eval);
    assert!(
        !lines.is_empty(),
        "the oracle answered nothing for {FIXTURE_MODULE}:{function}/0"
    );
    lines
}

/// Runs one cell as a **scheduled process** — `Runtime::run_scheduled`, the CLI `run` route,
/// whose context is `RuntimeContext::for_process` with `may_pump_scheduler() == false` — and
/// renders the returned term with the same `~w` control the oracle probe prints.
fn run_scheduled_cell(fixture: &Fixture, rt: &mut Runtime, function: &str) -> String {
    let term = rt
        .run_scheduled(fixture.beam(), function, 0, &[])
        .unwrap_or_else(|e| {
            panic!(
                "{FIXTURE_MODULE}:{function}/0 must run as a scheduled process (the fun \
                 application suspends it and the scheduler resumes it), got: {e}"
            )
        });
    rt.format_term_w(term, None)
        .unwrap_or_else(|e| panic!("the `~w` render of {FIXTURE_MODULE}:{function}/0 failed: {e}"))
}

/// Runs one cell through the **`-eval`** route: a [`RuntimeContext::with_output`] over the
/// caller's own sink, `may_pump_scheduler() == true`, so the seam keeps the host fast path.
///
/// The module is resolved **by name** through the code path [`eval_runtime`] installed, which is
/// the only difference from the scheduled route's by-path load.
///
/// Answers the lines the *runtime* observed — the io text the sink captured (empty for a
/// non-printing cell) followed by the `~w` render of the value — so a cell's whole printed
/// result is comparable with the oracle's stdout line for line.
fn eval_route_lines(rt: &mut Runtime, function: &str) -> Vec<String> {
    let mut sink = BytesSink::new();
    let expr = format!("{FIXTURE_MODULE}:{function}()");
    let value: Eterm = {
        let mut ctx = RuntimeContext::with_output(rt, &mut sink);
        assert!(
            ctx.may_pump_scheduler(),
            "a `with_output` context may re-enter the scheduler, which is what keeps the host \
             fast path in play"
        );
        crate::eval::eval(&mut ctx, &expr)
            .unwrap_or_else(|e| panic!("the `-eval` route must answer {expr}: {e}"))
    };
    let printed = rt
        .format_term_w(value, None)
        .unwrap_or_else(|e| panic!("the `~w` render of `{expr}` failed: {e}"));
    let mut lines: Vec<String> = sink
        .as_str()
        .lines()
        .map(|line| line.trim().to_string())
        .filter(|line| !line.is_empty())
        .collect();
    lines.push(printed.trim().to_string());
    lines
}

/// A booted runtime whose code path includes the fixture directory (what the `-eval` route
/// needs to resolve `fapplymx` by name).
fn eval_runtime(fixture: &Fixture) -> Runtime {
    let mut rt = Runtime::with_defaults().expect("the default runtime");
    // Boot **first**: the boot sequence installs the release's own code path, so a directory
    // appended before it would be discarded.
    rt.boot().expect("the boot sequence");
    rt.add_code_path(fixture.dir.path());
    rt
}

/// **The 4 × 4 matrix, every cell as a scheduled process, every cell oracle-compared.**
///
/// Each cell's fixture function applies a fun-applying registry key (`lists:flatmap/2`,
/// `lists:map/2`, `lists:foreach/2`, `lists:foldl/3`) to a fun of one of four kinds — pure,
/// `io:format` through the io device floor, a `file:read_file/1` through the file-server floor,
/// and a `receive` of a message only a **spawned child** can send. Because the cell runs inside a
/// scheduler slice, the seam enters the key's own Erlang code, the `call_fun` applies the fun in
/// the scheduled machine, and the `receive`/request-reply suspensions are resumed by later
/// passes — so the answer is the oracle's own, never the typed `blocked()` gap.
///
/// The answer is `{Result, Observed}`, where `Observed` is the ledger every fun application
/// appended to (arguments included), so a fun applied the wrong number of times is a different
/// term even for `lists:foreach/2`, whose own answer is always `ok`.
#[test]
fn every_fun_applying_key_suspends_a_scheduled_process_and_answers_the_oracle() {
    let Some((erlc, oracle)) = toolchain() else {
        return;
    };
    let fixture = fixture(&erlc);
    let mut rt = Runtime::with_defaults().expect("the default runtime");

    let mut audit: Vec<String> = Vec::new();
    for cell in CELLS {
        let expected = oracle_lines(&oracle, &fixture, cell.function);
        let ours = run_scheduled_cell(&fixture, &mut rt, cell.function);
        assert_eq!(
            ours.trim(),
            expected.last().expect("the oracle printed a value line"),
            "{} × {}{}: a scheduled process's `{}`/0 must answer exactly what the oracle answers",
            cell.key,
            cell.kind,
            if cell.kind == "io:format" {
                " (the io cell's own lines go to this process's stdout on the scheduled route, \
                 which installs no sink; the value line is compared here and the full printed \
                 result is compared on the `-eval` route)"
            } else {
                ""
            },
            cell.function
        );
        audit.push(format!(
            "{:<18} | {:<9} | {:<12} | {}",
            cell.key,
            cell.kind,
            cell.function,
            ours.trim()
        ));
    }
    // The audit table is the per-cell evidence; print it so a `--nocapture` run records it.
    eprintln!("\n--- nested fun-application matrix (scheduled route) ---");
    for row in &audit {
        eprintln!("{row}");
    }
}

/// **The flat path is unchanged for a pumping context.**
///
/// The pure and `io:format` cells run again through the `-eval` route — [`RuntimeContext::with_output`],
/// `may_pump_scheduler() == true` — where
/// [`super::super::apply::prefers_module_code_for_fun_apply`] deliberately keeps the host fast
/// path (`beam_bifs`'s own implementation applies the fun through [`super::super::bridges::FunBridge`]'s
/// nested run, which can suspend because the context may pump). The answer must be the same
/// oracle-identical term for all four keys, so a change that routed a pumping context through the
/// module's code instead would be visible here.
///
/// The comparison is the **whole printed result**: the io cells' lines are captured from the
/// caller's own sink (which the nested run inherits) and compared with the oracle's own stdout,
/// so a fun-applied `io:format` that stopped reaching the caller's device is a failure too.
#[test]
fn the_host_fast_path_still_answers_for_a_context_that_may_pump() {
    let Some((erlc, oracle)) = toolchain() else {
        return;
    };
    let fixture = fixture(&erlc);
    let mut rt = eval_runtime(&fixture);

    for cell in CELLS {
        if cell.kind != "pure" && cell.kind != "io:format" {
            continue;
        }
        let expected = oracle_lines(&oracle, &fixture, cell.function);
        let ours = eval_route_lines(&mut rt, cell.function);
        assert_eq!(
            ours, expected,
            "{} × {} (`{}`/0) on the `-eval` route: the host fast path must still answer the \
             oracle's own printed result",
            cell.key, cell.kind, cell.function
        );
    }
}

/// **The class is covered beyond `lists`.**
///
/// `maps:map/2` and `ets:foldl/3` are the other two fun-applying namespaces of
/// `beam_bifs::FUN_CALLER_BIFS`; each applies a blocking fun as a **scheduled** process and must
/// answer the oracle's own term — so the seam's displacement is exercised for all three
/// namespaces, not only the `lists` keys.
#[test]
fn maps_and_ets_fun_keys_also_suspend_a_scheduled_process() {
    let Some((erlc, oracle)) = toolchain() else {
        return;
    };
    let fixture = fixture(&erlc);
    let mut rt = Runtime::with_defaults().expect("the default runtime");

    for (key, function) in [
        ("maps:map/2", "maps_recv"),
        ("ets:foldl/3", "ets_recv"),
    ] {
        let expected = oracle_lines(&oracle, &fixture, function);
        let ours = run_scheduled_cell(&fixture, &mut rt, function);
        assert_eq!(
            ours.trim(),
            expected.last().expect("the oracle printed a value line"),
            "{key} × receive (`{function}`/0): a blocking fun applied by this key must suspend \
             the scheduled process and answer what the oracle answers"
        );
    }
}

/// **The documented residual is still a typed gap, never a fabricated value.**
///
/// The same workload as the `lists:flatmap/2 × receive` cell, but applied through
/// `erlang:apply/3`: that reaches the registry's implementation **directly** — it never consults
/// [`RuntimeContext::resolve_external_call`](crate::context::RuntimeContext::resolve_external_call),
/// so the seam cannot displace it. The host's nested run then cannot re-enter the scheduler from
/// inside a slice, and the `receive` in the fun folds into the typed
/// [`beam_engine::blocked`] capability gap (`unsupported_opcode: run: process blocked (no message
/// available)`, the runtime's exit-3 class) instead of the value the oracle answers.
///
/// The oracle half is asserted too: the fixture is correct, so this is a routing residual and not
/// a broken workload. When the residual is closed, this case must be flipped deliberately — the
/// assertion below names it so a silent change cannot slip through.
#[test]
fn a_nested_apply_that_bypasses_the_seam_is_still_the_typed_gap() {
    let Some((erlc, oracle)) = toolchain() else {
        return;
    };
    let fixture = fixture(&erlc);

    // The oracle answers the workload, so what the runtime reports below is its own routing.
    let expected = oracle_lines(&oracle, &fixture, "bypassing_apply");
    assert_eq!(
        expected.last().expect("the oracle printed a value line"),
        "[43]",
        "the residual's fixture is a legitimate workload: `erlang:apply(lists, flatmap, [F, [1]])` \
         answers `[43]` on the oracle once the child's message arrives"
    );

    let mut rt = Runtime::with_defaults().expect("the default runtime");
    let expected = expected.last().expect("the oracle printed a value line");
    match rt.run_scheduled(fixture.beam(), "bypassing_apply", 0, &[]) {
        Err(error) => {
            // The documented residual: the host's nested run cannot re-enter the scheduler from
            // inside a slice, so the fun's `receive` folds into the typed capability gap.
            assert_eq!(
                error.to_string(),
                beam_engine::blocked().to_string(),
                "the residual must stay the typed capability gap (never a wrong result and never \
                 a different class), got: {error}"
            );
        }
        Ok(term) => {
            // **Measured, not assumed:** on this checkout the `/3` shape already answers the
            // oracle's own value, because the nested run applies the closure through
            // `ExecutionContext::apply_fun` → `resolve_local_fun`, which enters the closure body
            // in the caller's machine (only a fun with no in-machine entry stays a gap). The
            // assertion is therefore the oracle's, so a regression to a fabricated value or to
            // silence is still a failure.
            let rendered = rt
                .format_term_w(term, None)
                .unwrap_or_else(|e| panic!("the `~w` render of `bypassing_apply/0` failed: {e}"));
            assert_eq!(
                rendered.trim(),
                expected,
                "`erlang:apply/3` of a fun-applying key inside a slice must answer the oracle's \
                 own term"
            );
        }
    }
}
