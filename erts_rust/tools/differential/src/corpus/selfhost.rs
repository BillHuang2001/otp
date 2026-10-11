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

//! Contracts **A.9 + A.2** — the two shapes of the self-hosted build's **in-process**
//! `compile:file/2`: the **E0 route** (`no_spawn_compiler_process`) and the **default,
//! scheduled-process** shape (the `erl -noshell -s compile …` / `erlc` shape).
//!
//! `../../../CONTEXT.md` (its "A.9 — what an in-process compile actually needs" section) is
//! the design source: OTP's `compile:file/2` path reaches only `kernel` + `stdlib` (`epp` — an
//! io-device-only client — plus `erl_features`, `erl_scan`/`erl_parse`/`erl_lint`/
//! `erl_expand_records`, the `v3_core` → `beam_*` passes and `save_binary`'s write), and its
//! acceptance test is a comparison of the produced `.beam` against an `erlc +deterministic`
//! build (two compilations of one source are byte-identical under `deterministic`). The E0
//! route keeps the whole compile in the **calling** process; the default shape spawns
//! `compile:file/2`'s compiler process, and the caller parks in the receive that waits for it.
//!
//! This group is the **evidence scaffold** for both routes: two module-run cases whose goldens
//! are the oracle's own integer lines, so each flips to `agree` automatically — with no corpus
//! edit — the moment the runtime hosts it.
//!
//! # The cases
//!
//! Each embedded module self-locates its temp dir (`filename:dirname(code:which(?MODULE))`),
//! stages its own source through the runtime's file-server **write** path, and then compiles
//! that source with `deterministic` and `{outdir, Dir}`, letting the compiler write the object
//! code into the case's outdir.
//!
//! * `case()` — the E0 route (`no_spawn_compiler_process`): it reports the integer `1` once the
//!   produced `<outdir>/selfhost.beam` reads back as a non-empty binary, and a distinct integer
//!   otherwise.
//! * `sched_case()` — the default shape: it reports `erlang:phash2/1` of the produced `.beam`'s
//!   `Code` chunk (read back with `beam_lib:chunks/2`), the integer both runtimes must agree on
//!   because `deterministic` makes two compilations of one source byte-identical.
//!
//! In both cases the compile's own failure is deliberately **not** caught: a runtime that
//! cannot host the route must raise its typed capability gap (the CLI then exits `3`), never a
//! plausible-looking integer.
//!
//! # Why they are `unsupported` on the Rust runtime today
//!
//! `beam_rt` does not host either route yet, and the honest classification of that gap is the
//! whole point of the cases. The `compiler` application is on the runtime's code path and the
//! native `file_server_2` serves the route's `{write_file, …}` request, so the routes' only
//! obstacle is their **scheduled-process** shape: a parked receive-window collection relocates
//! the receiver heap against an incomplete root set (a live `Machine::pending_exception` is
//! unpublished). The runtime reports that typed gap
//! (`beam_rt: unsupported: opcode: run: process blocked (no message available)`, exit `3`), so
//! both cases classify as `unsupported` — never a mismatch — and are recorded in
//! `known-gaps.txt`.
//!
//! # What they will prove once the routes land
//!
//! Both runtimes then execute the same compiled wrapper: the oracle loads the real `compiler`
//! application (a cross-module call into an installed OTP application, contract A.7 territory)
//! and the runtime its own in-process compiler. Three contracts are in play and exactly two are
//! claimed: [`Contract::BeamAsm`] (a real `.beam` executes and object code is produced) and
//! [`Contract::Build`] (the build/bootstrapping route itself). [`Contract::OtpApps`] is **not**
//! claimed — the routes' whole point is that they reach only `kernel` + `stdlib`, so nothing
//! about them depends on the wider application surface.

use super::{Case, CaseKind, Contract, Golden, ModuleRun};

/// The embedded module of the E0 route: `no_spawn_compiler_process`, one integer result.
///
/// Shape rules, all load-bearing:
///
/// * the `raw` source read keeps the staging step's file-service surface to the write alone
///   (see the module docs), and the write comes **before** the compile so a runtime that
///   cannot serve it reports its typed file-service gap up front rather than the compile's
///   own raise;
/// * `no_spawn_compiler_process` keeps the compile in the calling process, and `deterministic`
///   makes its object code path-independent (so a byte comparison against an
///   `erlc +deterministic` build stays valid);
/// * `{outdir, Dir}` is the outdir the route names — the same directory the compiled
///   `.beam` was loaded from, so the compiler's own `save_binary` writes the produced object
///   code next to the running module;
/// * the compile result is **not** wrapped in a `catch`: the raise must escape.
const SELFHOST_SOURCE: &str = r#"-module(selfhost).
-export([compile_self/0]).

%% E0 (contract A.9): recompile this module's own source IN-PROCESS and confirm the object
%% code it produces. `compile:file/2` reaches only `kernel` + `stdlib`, and with
%% `{outdir, Dir}` its last pass (`save_binary`) writes `<outdir>/selfhost.beam` through the
%% file server — so the route is staged write-first: stage the source, then compile it.
compile_self() ->
    Dir = filename:dirname(code:which(?MODULE)),
    Source = filename:join(Dir, "selfhost.erl"),
    Staged = filename:join(Dir, "selfhost.staged.erl"),
    {ok, Src} = file:read_file(Source, [raw]),
    ok = file:write_file(Staged, Src),
    case compile:file(Source, [no_spawn_compiler_process, deterministic, {outdir, Dir}]) of
        {ok, _Mod, Bin} when is_binary(Bin) -> 1;
        {ok, _Mod} -> produced(filename:join(Dir, "selfhost.beam"));
        _ -> 3
    end.

%% The `{outdir, Dir}` (file-writing) form of the route: confirm the produced object code.
produced(Beam) ->
    case file:read_file(Beam) of
        {ok, Bin} when is_binary(Bin), byte_size(Bin) > 0 -> 1;
        _ -> 2
    end.
"#;

/// The embedded module of the default (scheduled-process) shape: one integer result.
///
/// It differs from [`SELFHOST_SOURCE`] in exactly two ways, both load-bearing:
///
/// * no `no_spawn_compiler_process`, so `compile:file/2` spawns its own compiler process and
///   the caller parks in the receive that waits for it — the `erl -noshell -s compile …` /
///   `erlc` shape;
/// * its observable is the produced object code itself: the `.beam` the route wrote into
///   `<outdir, Dir>` is read back and reduced to `erlang:phash2/1` of its `Code` chunk
///   (through `beam_lib:chunks/2`). `deterministic` makes two compilations of one source
///   byte-identical, so that integer is a stable module-run golden (and, once the host runs
///   the compiler, a real `.beam`-parity assertion rather than a mere "it returned").
const SELFHOST_SCHED_SOURCE: &str = r#"-module(selfhost_sched).
-export([compile_self/0]).

%% The DEFAULT (contract A.9) route: recompile this module's own source WITHOUT
%% `no_spawn_compiler_process`, so `compile:file/2` spawns its compiler process while this one
%% parks in the receive that waits for it — the `erl -noshell -s compile …` / `erlc` shape.
%% `{outdir, Dir}` makes its last pass (`save_binary`) write `<outdir>/selfhost_sched.beam`
%% through the file server. The observable is `phash2/1` of the produced `.beam`'s `Code`
%% chunk: `deterministic` makes two compilations of one source byte-identical, so the integer
%% is stable.
compile_self() ->
    Dir = filename:dirname(code:which(?MODULE)),
    Source = filename:join(Dir, "selfhost_sched.erl"),
    Staged = filename:join(Dir, "selfhost_sched.staged.erl"),
    {ok, Src} = file:read_file(Source, [raw]),
    ok = file:write_file(Staged, Src),
    {ok, _Mod} = compile:file(Source, [deterministic, {outdir, Dir}]),
    Beam = filename:join(Dir, "selfhost_sched.beam"),
    {ok, {?MODULE, [{"Code", Code}]}} = beam_lib:chunks(Beam, ["Code"]),
    erlang:phash2(Code).
"#;

/// The E0 route case: the in-process `compile:file/2` (`no_spawn_compiler_process`).
fn case() -> Case {
    Case {
        id: "selfhost/in_process_compile_file_no_spawn_deterministic",
        description: "the A.9 E0 route: recompile the module's own source in-process \
                      (no_spawn_compiler_process, deterministic, {outdir, Dir}) and read the \
                      produced .beam back",
        contracts: &[Contract::BeamAsm, Contract::Build],
        kind: CaseKind::Module(ModuleRun {
            source: SELFHOST_SOURCE,
            module: "selfhost",
            function: "compile_self",
            arity: 0,
            args: &[],
            nif: None,
        }),
        golden: Golden {
            stdout: Some("1"),
            exit_code: 0,
        },
    }
}

/// The default (scheduled-process) shape case: `compile:file/2` with its own compiler process.
fn sched_case() -> Case {
    Case {
        id: "selfhost/scheduled_process_compile_file_deterministic",
        description: "the A.9 default shape: recompile the module's own source through \
                      compile:file/2's own compiler process (deterministic, {outdir, Dir}) and \
                      report erlang:phash2/1 of the produced .beam's Code chunk",
        contracts: &[Contract::BeamAsm, Contract::Build],
        kind: CaseKind::Module(ModuleRun {
            source: SELFHOST_SCHED_SOURCE,
            module: "selfhost_sched",
            function: "compile_self",
            arity: 0,
            args: &[],
            nif: None,
        }),
        golden: Golden {
            stdout: Some("7806190"),
            exit_code: 0,
        },
    }
}

/// The A.9 self-hosted-build group: the two `compile:file/2` shapes.
pub(crate) fn group() -> Vec<Case> {
    vec![case(), sched_case()]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The module-run invocation of one case, panicking when it is not a module-run case.
    fn run_of(case: &Case) -> &ModuleRun {
        let CaseKind::Module(run) = &case.kind else {
            panic!("case {} is not a module-run case", case.id);
        };
        run
    }

    #[test]
    fn the_group_is_the_two_self_hosted_build_routes() {
        let cases = group();
        assert_eq!(cases.len(), 2, "the group is the two compile:file/2 shapes");
        assert_eq!(
            cases[0].id,
            "selfhost/in_process_compile_file_no_spawn_deterministic"
        );
        assert_eq!(
            cases[1].id,
            "selfhost/scheduled_process_compile_file_deterministic"
        );
        for case in &cases {
            assert_eq!(case.contracts, &[Contract::BeamAsm, Contract::Build]);
            assert_eq!(case.golden.exit_code, 0);
            assert!(
                case.golden
                    .stdout
                    .is_some_and(|g| g.chars().all(|c| c.is_ascii_digit())),
                "case {}: a module-run golden is a plain integer line",
                case.id
            );
            let run = run_of(case);
            assert_eq!(run.function, "compile_self");
            assert_eq!(run.arity as usize, run.args.len());
            assert!(run.nif.is_none(), "the routes need no C/NIF fixture");
        }
        // Each case runs its **own** module, so the two temp dirs never collide.
        assert_eq!(run_of(&cases[0]).module, "selfhost");
        assert_eq!(run_of(&cases[1]).module, "selfhost_sched");
        assert_eq!(cases[0].golden.stdout, Some("1"));
        assert_eq!(cases[1].golden.stdout, Some("7806190"));
    }

    #[test]
    fn the_module_source_pins_the_e0_route() {
        let cases = group();
        let run = run_of(&cases[0]);
        for needle in [
            "-module(selfhost).",
            "code:which(?MODULE)",
            "file:write_file(Staged, Src)",
            "compile:file(Source, [no_spawn_compiler_process, deterministic, {outdir, Dir}])",
        ] {
            assert!(run.source.contains(needle), "the route needs {needle:?}");
        }
        // The staging write must precede the compile: behind a file-server read the runtime
        // reports the block instead of the write's typed capability gap (so the case could
        // no longer be classified, only mismatched).
        let write = run.source.find("file:write_file(Staged").expect("staging");
        let compile = run.source.find("compile:file(Source").expect("compile");
        assert!(write < compile, "the route is staged write-first");
        // The compile's own failure must escape: the runtime's typed capability gap (CLI
        // exit 3) is the only honest signal for a runtime that cannot host the route.
        assert!(!run.source.contains("catch"), "the raise must not be caught");
    }

    #[test]
    fn the_scheduled_shape_drops_no_spawn_and_reports_the_code_chunk_hash() {
        let cases = group();
        let run = run_of(&cases[1]);
        for needle in [
            "-module(selfhost_sched).",
            "code:which(?MODULE)",
            "file:write_file(Staged, Src)",
            "compile:file(Source, [deterministic, {outdir, Dir}])",
            "beam_lib:chunks(Beam, [\"Code\"])",
            "erlang:phash2(Code).",
        ] {
            assert!(run.source.contains(needle), "the route needs {needle:?}");
        }
        // The shape is the *default* `compile:file/2`: without the option the compiler runs in
        // its own process (the `erl -noshell -s compile …` / `erlc` shape) — that spawn is the
        // whole difference from the E0 case.
        assert!(
            !run.source.contains("[no_spawn_compiler_process"),
            "the default shape must not keep the compile in the calling process"
        );
        // The staging write must precede the compile (see the E0 case), and the compile must
        // be observed through the object code it wrote.
        let write = run.source.find("file:write_file(Staged").expect("staging");
        let compile = run.source.find("compile:file(Source").expect("compile");
        let chunks = run.source.find("beam_lib:chunks").expect("code chunk");
        assert!(write < compile && compile < chunks, "staged write-first, then read back");
        // The compile's own failure must escape (see the E0 case).
        assert!(!run.source.contains("catch"), "the raise must not be caught");
    }
}
