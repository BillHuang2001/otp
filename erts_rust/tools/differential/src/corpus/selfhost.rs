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

//! Contracts **A.9 + A.2** — the **E0 route** of the self-hosted build: an *in-process*
//! `compile:file/2`.
//!
//! `../../../CONTEXT.md` (its "A.9 — what an in-process compile actually needs" section) is
//! the design source: the E0 route is `compile:file(File, [no_spawn_compiler_process,
//! deterministic, {outdir, Dir}])`, run in the **calling** process (so the whole compile
//! stays in the pumping process the `run` action drives), and its acceptance test is a
//! comparison of the produced `.beam` against an `erlc +deterministic` build (two
//! compilations of one source are byte-identical under `+deterministic`). The same analysis
//! names the two narrow blockers, both of which this case makes observable:
//!
//! * the `compiler` application is not on the runtime's default code path (the runtime
//!   serves `stdlib`/`kernel`/`erts` only), and
//! * the native `file_server_2` does not serve the `{write_file, …}` request the route's own
//!   `save_binary` pass makes when it writes the object code into the outdir.
//!
//! This group is the **evidence scaffold** for that route: one module-run case whose golden
//! is the oracle's own integer line, so the case flips to `agree` automatically — with no
//! corpus edit — the moment the runtime hosts the route.
//!
//! # The case
//!
//! The embedded module self-locates its temp dir (`filename:dirname(code:which(?MODULE))`),
//! stages its own source through the runtime's file-server **write** path, and then compiles
//! that source **in the calling process** (`no_spawn_compiler_process`) with `deterministic`,
//! letting the compiler write the object code into the case's outdir (`{outdir, Dir}`). It
//! reports the integer `1` only for a real result — `{ok, Mod, Bin}` with a binary `Bin`, or
//! the file-writing `{ok, Mod}` form once the produced `<outdir>/selfhost.beam` reads back
//! as a non-empty binary — and a distinct integer otherwise. The compile's own failure is
//! deliberately **not** caught: a runtime that cannot host the route must raise its typed
//! capability gap (the CLI then exits `3`), never a plausible-looking integer.
//!
//! # Why it is `unsupported` on the Rust runtime today
//!
//! `beam_rt` does not host the route yet, and the honest classification of that gap is the
//! whole point of the case. A bare `compile:*` call classifies as a **mismatch**, not a
//! capability gap: the `compiler` application is not on the runtime's code path, so the call
//! raises the bare `error:undef` the oracle raises for any missing module — a *plain Erlang
//! reason*, which the harness must report as a differing observation. So the scaffold asks
//! for the route's **file-service write** first (the `file:write_file/2` request the route's
//! `save_binary` pass makes): that is a gap the runtime genuinely signals with its typed
//! `unsupported` convention (`beam_rt: unsupported: file_server: the 'write_file/2' request
//! is not implemented`, exit `3`), so the case classifies as `unsupported` — never a
//! mismatch — and is recorded in `known-gaps.txt`.
//!
//! The source is read back over the **`raw`** (NIF-level) path so that the staging step's
//! only *file-service* request is that write: with a file-server read in between, the
//! runtime's single engine pass reports the resulting block instead of the write's typed
//! reason.
//!
//! # What it will prove once the route lands
//!
//! Both runtimes then execute the same compiled wrapper: the oracle loads the real
//! `compiler` application (a cross-module call into an installed OTP application, contract
//! A.7 territory) and the runtime its own in-process compiler; the observable is the single
//! integer `1` — the object code was produced into the outdir the route names. Three
//! contracts are therefore in play and exactly two are claimed: [`Contract::BeamAsm`]
//! (a real `.beam` executes and object code is produced) and [`Contract::Build`] (the
//! build/bootstrapping route itself). [`Contract::OtpApps`] is **not** claimed — the route's
//! whole point is that it reaches only `kernel` + `stdlib`, so nothing about it depends on
//! the wider application surface.

use super::{Case, CaseKind, Contract, Golden, ModuleRun};

/// The embedded module: the E0 route, one integer result.
///
/// Shape rules, all load-bearing:
///
/// * the `raw` source read keeps the staging step's file-service surface to the write alone
///   (see the module docs), and the write comes **before** the compile so a runtime that
///   cannot serve it reports its typed gap up front rather than an opaque `undef` from the
///   un-loadable `compiler` module;
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

/// The single E0 case: the in-process `compile:file/2` route.
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

/// The A.9 E0 group: the in-process `compile:file/2` route.
pub(crate) fn group() -> Vec<Case> {
    vec![case()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_group_is_the_single_e0_route_case() {
        let cases = group();
        assert_eq!(cases.len(), 1, "the E0 scaffold is one case");
        let case = &cases[0];
        assert_eq!(
            case.id,
            "selfhost/in_process_compile_file_no_spawn_deterministic"
        );
        assert_eq!(case.contracts, &[Contract::BeamAsm, Contract::Build]);
        assert_eq!(case.golden.stdout, Some("1"));
        assert_eq!(case.golden.exit_code, 0);
        let CaseKind::Module(run) = case.kind else {
            panic!("the E0 route is a module-run case");
        };
        assert_eq!(run.module, "selfhost");
        assert_eq!(run.function, "compile_self");
        assert_eq!(run.arity as usize, run.args.len());
        assert!(run.nif.is_none(), "the route needs no C/NIF fixture");
    }

    #[test]
    fn the_module_source_pins_the_e0_route() {
        let CaseKind::Module(run) = group()[0].kind else {
            panic!("the E0 route is a module-run case");
        };
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
}
