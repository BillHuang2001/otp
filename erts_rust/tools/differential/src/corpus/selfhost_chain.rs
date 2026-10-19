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

//! Contract **A.9** — the *staged* self-hosted chain: the hand-off **between** toolchain stages.
//!
//! [`super::selfhost`] pins single in-process `compile:file/2` routes and artifact identity: one
//! stage, one call, one produced `.beam`. This group pins what that group cannot — a stage the
//! runtime compiled being **used** to drive the next stage, and the explicit
//! `epp` → `erl_scan` → `erl_lint` → `compile:forms/2` seam underneath the `compile:file/2`
//! shortcut. The distinction is the whole of contract A.9's "the runtime can run OTP's own
//! toolchain": compiling a module is only half of it; the compiled tool has to *do* the next
//! compile.
//!
//! `../../../CONTEXT.md` (its "A.9 — in-process compilation" section) records the claim this
//! group is the evidence for: a staged `epp → erl_scan → erl_lint → compile` chain is
//! byte-identical end to end on the Rust runtime, but that was pinned only by an in-crate test
//! (`crates/beam_rt`), never by a corpus case. Every case here is a **hand-off**:
//!
//! * [`staged_relation_case`] — the **stage seam**: `lib/stdlib/src/dets_sup.erl` goes through
//!   `epp:parse_file/2` (the front end), `erl_lint:module/1` (the lint stage) and
//!   `erl_scan:string/1` (the token stage separately, so the scanner is exercised as a stage of
//!   its own) and reaches `compile:forms/2` as a form list — *not* through `compile:file/2`.
//!   The produced artifact is then compared against the one `compile:file/2` produces from the
//!   identical source: the `Code` chunk, the export table and every md5-significant chunk must
//!   be identical, and the source's `-behaviour(supervisor)` must survive into the `attributes`
//!   chunk. This is the case that pins "the explicit staged route and the shortcut agree".
//! * [`staged_call_case`] — the **compile → load → run hand-off** of that staged route:
//!   `lib/stdlib/src/erl_internal.erl` (erl_lint's own operator/type-test dependency table) is
//!   parsed, linted, compiled, `beam_lib:md5/1`-checked, loaded with `code:load_binary/3` under
//!   a name the installation does not own, and *called* — eight of its exported predicates, so
//!   the freshly loaded copy's answers (both `true` and `false` ones) are the observable.
//! * [`compiler_pass_case`] — **the compiler compiling one of the compiler's own passes**:
//!   `lib/compiler/src/erl_bifs.erl` (the pass that answers "is this call pure / safe / an exit
//!   BIF?" for `sys_core_fold` and the SSA passes) is compiled *by* the runtime, loaded as
//!   `selfhost_chain_erl_bifs`, and the **freshly loaded copy** is asked eight of those
//!   questions — both polarities of each table — so the answers the compiler's own analysis
//!   gives on this runtime are the observable. `erl_bifs` makes no self-qualified call, so every
//!   internal call stays inside the fresh copy.
//! * [`compiled_scan_case`] — the same claim for a *producing* stage: `lib/stdlib/src/erl_scan.erl`
//!   (2318 source lines) is compiled and loaded, and the fresh scanner tokenizes a module's
//!   source; those tokens go through `erl_parse:parse_form/1`, `compile:forms/2`,
//!   `code:load_binary/3` and a call — the fresh stage feeding the rest of the chain.
//! * [`two_stage_case`] — the **three-hop chain**: the case compiles module A in-process and
//!   loads it, then *calls* A, and A compiles module B in-process (its own `epp` +
//!   `compile:forms/2`), loads B and calls B. Four hops — case → A → B → value — where only the
//!   first and last are visible to the case itself.
//! * [`moduledoc_file_case`] — `epp`'s **file-borne documentation** read through the *staged*
//!   route: a module whose `-moduledoc({file, "chain_moduledoc.txt"})` names a sibling file is
//!   parsed by `epp:parse_file/2` (which must read that file through the file server) and
//!   compiled with `compile:forms/2`; the produced `Docs` chunk's module doc must be exactly the
//!   staged text, and the module must then load and run.
//! * [`option_list_case`] — the **explicit option list** of the forms route: `[binary]`,
//!   `[binary, return_errors, return_warnings, debug_info]`, `[binary, report]` and
//!   `compile:noenv_forms(Forms, [binary, return_errors])` return the documented `{ok, Mod, Bin}`
//!   versus `{ok, Mod, Bin, Warns}` **tuple shapes**, all three producing the same `Code` chunk,
//!   and the debug_info image declaring the loader's mandatory chunk set.
//!
//! # Why the goldens are relations and classified shapes, never hashes
//!
//! The same rule as `selfhost`'s applies: a `.beam` hash (or a byte length) is a
//! *compiler-release* artifact, and this corpus' oracle is whichever OTP release the host
//! installed — a different one in CI (OTP 28) than on a developer host (OTP 29). Every
//! observable below is therefore an **equality between two artifacts the same runtime produced
//! in the same run**, a *source-derived* projection of the artifact (the export table), or a
//! classified shape (`three`/`{four, []}`, the `file` load-state tag, a token-category list).
//!
//! # Chain shapes measured and deliberately **not** pinned
//!
//! Each of the following was measured on both runtimes while this group was written; none is a
//! corpus case, and the reason is recorded so a later pass does not re-measure it:
//!
//! * **`beam_lib:md5/1` is *not* equal between the staged forms route and the `compile:file/2`
//!   route** — not a divergence: both runtimes produce the same two images, and the difference
//!   is a property of OTP's compiler, not of the runtime. The file route's image carries a
//!   `Meta` chunk (`[{enabled_features,[maybe_expr]}]`) that the forms route's image does not,
//!   and `Meta` is one of `beam_lib`'s md5-significant chunks. [`staged_relation_case`] therefore
//!   pins the relation at the **md5-significant chunk list minus `Meta`** (every other md5
//!   chunk — `Atom`/`AtU8`/`Code`/`StrT`/`ImpT`/`ExpT`/`FunT`/`LitT`/`Recs`/`DbgB` — is
//!   identical), which is the strongest *release-independent* form of the same claim. Pinning
//!   `md5(A) =/= md5(B)` would be pinning a compiler-release detail.
//! * **`compile:string/2,3` does not exist** in the installed stdlib (`undef` on both sides), so
//!   a "compile from a string" hop has no observable at all. The scan→parse→forms chain of
//!   [`compiled_scan_case`] is the shape that does exist.
//! * **`compile:forms(Forms, [report])` prints diagnostics to stdout** (measured: `:5: Warning:
//!   function unused/0 is unused`), which would put a compiler diagnostic *before* the probe's
//!   single `ok …` line — an error-logger formatting detail, not a runtime property.
//!   [`option_list_case`] therefore exercises `report` on **clean** forms only, where it is
//!   silent, and the printing route stays unpinned. (`return_warnings` still makes the return a
//!   4-tuple when `report` is present — measured; only the clean shapes are pinned.)
//! * **Loading a freshly compiled artifact under a *stdlib* module name is refused on both
//!   sides** (`{error, sticky_directory}` — `selfhost`'s module docs record the measurement), so
//!   every case here that *loads* what it compiled first **renames the module attribute**, which
//!   is also what makes the hand-off observable: the module that runs is the one this run
//!   produced, under a name no installation owns.
//! * **The staged route over a large source through the module-run argv is too expensive**: the
//!   runtime's freshly *spawned* processes run ~9–16× slower than the calling process (an A.9
//!   divergence recorded in `divergences.md`), and `selfhost` measured the `ordsets` compile at
//!   ≈7.6 s (module-run) against ≈0.8 s (`-eval`). The two large-source cases
//!   ([`compiler_pass_case`], [`compiled_scan_case`]) are therefore **expression** cases; the two
//!   module-run cases here ([`two_stage_case`], [`moduledoc_file_case`]) compile only 3–4-line
//!   staged modules, which keeps their spawn cost negligible.
//! * **`erl_lint` itself is compiled on request but not as a corpus case**: compiling the real
//!   `lib/stdlib/src/erl_lint.erl` (5899 source lines) in-process, loading the fresh copy as
//!   `selfhost_chain_lint` and having **it** lint three form lists (a warnable one, a clean one
//!   and one reading an unbound variable) **agrees** on both runtimes — the fresh linter answers
//!   `[{3,erl_lint,{unused_var,'X'}}]`, `[]` and `{[{5,erl_lint,{unbound_var,'Z'}}],[]}` — but
//!   the compile alone costs ≈**164 s** on the Rust runtime against ≈1.3 s on the oracle
//!   (measured; the compiler runs ~50× slower here on a large source, an execution-engine
//!   property, not a divergence), which is far past what a gate case may carry. The
//!   *compiler-pass* case ([`compiler_pass_case`]) pins the same "a fresh copy of the compiler's
//!   own code answers the questions" claim on a pass that compiles in ≈1 s, and the measurement
//!   is recorded in `divergences.md` § A.9 so the route is not lost.
//! * **The `erl_scan` case is the group's one expensive case** and is deliberately an expression
//!   case for that reason: the compile costs ≈**36 s** on the Rust runtime against ≈0.7 s on the
//!   oracle (measured), while the same compile through the module-run argv would pay the
//!   freshly-spawned-process penalty on top (≈9–16×, `divergences.md` § A.9). 2318 source lines
//!   is the smallest source that still is the *real* scanner; the group keeps no larger one.
//! * **A `{moduledoc, {file, …}}` source cannot be an expression case**: `epp` resolves the named
//!   file relative to the source's own directory, so the source must exist on disk next to it —
//!   the module-run case directory, never a repo-relative path (writing into the repository root
//!   would be a side effect of running the gate). [`moduledoc_file_case`] is the module-run
//!   shape for exactly that reason, and it stages both files through the runtime's own write path.
//!
//! # Status
//!
//! All seven cases **agree** on the Rust runtime, and every golden is the oracle's own stdout
//! line (see the `MEASURED` table in the tests below). Two contracts are claimed:
//! [`Contract::BeamAsm`] (a real `.beam` is produced, loaded and executed) and
//! [`Contract::Build`] (the build/bootstrapping route itself). [`Contract::OtpApps`] is **not**
//! claimed: every route here reaches only `kernel` + `stdlib`, exactly like the `selfhost` group.

use super::{Case, CaseKind, Contract, Golden, ModuleRun, Probe};

/// The **stage-seam** relation: the explicit `epp` → `erl_lint` → `erl_scan` → `compile:forms/2`
/// chain against `compile:file/2` on the identical source.
///
/// Shape rules, all load-bearing:
///
/// * the source is addressed **repo-relatively** (`lib/stdlib/src/dets_sup.erl`): the harness
///   spawns both runtimes with the repository root as their working directory, and it is the
///   smallest stdlib module carrying a `-behaviour` attribute (42 source lines), so the case
///   stays cheap on both sides;
/// * the three stages are invoked **explicitly and separately**, not through `compile:file/2`:
///   `epp:parse_file/2` produces the form list, `erl_lint:module/1` lints it (returning
///   `{ok, Warnings}`), and `erl_scan:string/1` tokenizes a snippet of its own, so the scanner is
///   pinned as a stage rather than as an internal of `epp`. The token categories are the
///   observable, never the token annotations (a location encoding is a release detail);
/// * the comparison against the shortcut is on the **object code**: the `Code` chunk and the
///   export table must be identical, and the md5-significant chunk list (minus `Meta`, see the
///   module docs) must be identical too — an artifact-level equality, not a status code;
/// * no hash *value* and no byte length is pinned, and the raise is never caught: a runtime that
///   cannot host the route must raise its typed capability gap (the CLI then exits `3`).
const STAGED_RELATION_EXPR: &str = r#"begin File = "lib/stdlib/src/dets_sup.erl", {ok, Forms} = epp:parse_file(File, []), {ok, LintWarnings} = erl_lint:module(Forms), {ok, Toks, _} = erl_scan:string("twice(N) -> 2 * N."), {ok, dets_sup, Bin} = compile:forms(Forms, [binary, no_spawn_compiler_process, deterministic]), {ok, {dets_sup, [{exports, Exports}]}} = beam_lib:chunks(Bin, [exports]), {ok, {dets_sup, [{"Code", Code}]}} = beam_lib:chunks(Bin, ["Code"]), {ok, dets_sup, FileBin} = compile:file(File, [binary, no_spawn_compiler_process, deterministic]), {ok, {dets_sup, [{"Code", FileCode}]}} = beam_lib:chunks(FileBin, ["Code"]), {ok, {dets_sup, [{exports, FileExports}]}} = beam_lib:chunks(FileBin, [exports]), {ok, {dets_sup, [{attributes, Attrs}]}} = beam_lib:chunks(Bin, [attributes]), Md5Ids = ["Atom", "AtU8", "Code", "StrT", "ImpT", "ExpT", "FunT", "LitT", "Recs", "DbgB"], Sig = fun(Image) -> {ok, dets_sup, Chunks} = beam_lib:all_chunks(Image), [Data || Id <- Md5Ids, {_, Data} <- [lists:keyfind(Id, 1, Chunks)]] end, {LintWarnings, [element(1, T) || T <- Toks], Exports =:= FileExports, Code =:= FileCode, Sig(Bin) =:= Sig(FileBin), proplists:get_value(behaviour, Attrs)} end"#;

/// The **compile → load → run** hand-off of the staged route, on `erl_internal`.
///
/// `lib/stdlib/src/erl_internal.erl` is the module `erl_lint` itself consults for "is `+`/2 an
/// arithmetic operator?", "is `size`/1 a guard BIF?", "is `is_atom`/1 a type test?" — so the
/// runtime is made to compile, lint, load and **call** the lint stage's own dependency table.
///
/// Shape rules, all load-bearing:
///
/// * the module attribute is **renamed** (`erl_internal` → `selfhost_chain_internal`) for the
///   reason `selfhost`'s module docs record: a stdlib name resolves to the installation's own
///   (sticky) copy, and the rename is the only way the freshly compiled artifact can be the
///   module that runs. `erl_internal` makes no self-qualified call (`grep 'erl_internal:'`
///   finds none), so the rename cannot smuggle a call back to the installed module;
/// * the explicit `erl_lint:module/1` stage runs on the *renamed* forms, so the lint stage sees
///   exactly the list the compiler will see;
/// * `beam_lib:md5/1` is pinned by its **shape** only (a 16-byte image) — the value is a
///   compiler-release artifact;
/// * the observable is the freshly loaded copy's **answers**: both arities of the operator
///   tables, a BIF name that exists and one that does not, a guard BIF and a type test — so a
///   runtime that loaded the wrong module (or none) cannot answer the golden;
/// * the raise is not caught: a runtime that cannot host the route reports its typed gap.
const STAGED_CALL_EXPR: &str = r#"begin File = "lib/stdlib/src/erl_internal.erl", {ok, Forms0} = epp:parse_file(File, []), Forms = [case Form of {attribute, Anno, module, erl_internal} -> {attribute, Anno, module, selfhost_chain_internal}; _ -> Form end || Form <- Forms0], {ok, LintWarnings} = erl_lint:module(Forms), {ok, selfhost_chain_internal, Bin} = compile:forms(Forms, [binary, no_spawn_compiler_process, deterministic]), {ok, {selfhost_chain_internal, Md5}} = beam_lib:md5(Bin), {module, selfhost_chain_internal} = code:load_binary(selfhost_chain_internal, "selfhost_chain_internal.beam", Bin), {LintWarnings, byte_size(Md5), selfhost_chain_internal:arith_op('+', 2), selfhost_chain_internal:arith_op('foo', 2), selfhost_chain_internal:bool_op('and', 2), selfhost_chain_internal:bool_op('andalso', 2), selfhost_chain_internal:comp_op('=:=', 2), selfhost_chain_internal:bif(atom_to_list, 1), selfhost_chain_internal:bif(nonsense, 3), selfhost_chain_internal:guard_bif(size, 1), selfhost_chain_internal:type_test(is_atom, 1)} end"#;

/// **The compiler compiling one of the compiler's own passes**: the runtime compiles
/// `erl_bifs`, and the fresh copy answers the compiler's purity/safety questions.
///
/// Shape rules, all load-bearing:
///
/// * the subject is the real `lib/compiler/src/erl_bifs.erl` — the pass `sys_core_fold` and the
///   SSA passes consult for "is this call pure?", "is it safe to evaluate early?", "is it an exit
///   BIF?", so this is the compiler's own analysis code being parsed and compiled by the runtime
///   it is about to run on;
/// * the module attribute is renamed (`erl_bifs` → `selfhost_chain_erl_bifs`): the installation
///   owns `erl_bifs`, and the rename is what lets the *fresh* copy be loaded and called.
///   `erl_bifs` contains no self-qualified call and no `?MODULE`, so every internal call of the
///   fresh copy stays inside the fresh copy;
/// * the observable is the fresh copy's **answers**, in both polarities of each of the three
///   tables (`is_pure`/`is_safe`/`is_exit_bif`), so a runtime that loaded the wrong module — or
///   none — cannot produce the golden;
/// * no hash, no byte length and no timing is pinned; the raise is not caught (a runtime that
///   cannot compile the pass reports its typed gap).
const COMPILER_PASS_EXPR: &str = r#"begin File = "lib/compiler/src/erl_bifs.erl", {ok, Forms0} = epp:parse_file(File, []), Forms = [case F of {attribute, A, module, erl_bifs} -> {attribute, A, module, selfhost_chain_erl_bifs}; _ -> F end || F <- Forms0], {ok, selfhost_chain_erl_bifs, Bin} = compile:forms(Forms, [binary, no_spawn_compiler_process, deterministic]), {module, selfhost_chain_erl_bifs} = code:load_binary(selfhost_chain_erl_bifs, "selfhost_chain_erl_bifs.beam", Bin), {selfhost_chain_erl_bifs:is_pure(erlang, abs, 1), selfhost_chain_erl_bifs:is_pure(erlang, put, 2), selfhost_chain_erl_bifs:is_safe(erlang, is_atom, 1), selfhost_chain_erl_bifs:is_safe(erlang, put, 2), selfhost_chain_erl_bifs:is_exit_bif(erlang, exit, 1), selfhost_chain_erl_bifs:is_exit_bif(erlang, abs, 1)} end"#;

/// **The fresh scanner drives the next stage**: `erl_scan` compiled by the runtime, then used to
/// tokenize the source of the next module, which is parsed, compiled, loaded and called.
///
/// The subject is the real `lib/stdlib/src/erl_scan.erl` (2318 source lines), renamed
/// `selfhost_chain_scan` for the loadability reason the module docs record (`erl_scan` makes no
/// self-qualified call either). The scanned module is deliberately a *three-form* module
/// (`-module`, `-export`, one function) fed form by form, so the chain is
/// fresh scanner → `erl_parse:parse_form/1` → `compile:forms/2` → `code:load_binary/3` → call,
/// and the observable is the token-category list plus the value the scanned module computes.
const COMPILED_SCAN_EXPR: &str = r#"begin File = "lib/stdlib/src/erl_scan.erl", {ok, Forms0} = epp:parse_file(File, []), Forms = [case F of {attribute, A, module, erl_scan} -> {attribute, A, module, selfhost_chain_scan}; _ -> F end || F <- Forms0], {ok, selfhost_chain_scan, Bin} = compile:forms(Forms, [binary, no_spawn_compiler_process, deterministic]), {module, selfhost_chain_scan} = code:load_binary(selfhost_chain_scan, "selfhost_chain_scan.beam", Bin), {ok, Toks1, _} = selfhost_chain_scan:string("-module(chain_scanned).", 1), {ok, F1} = erl_parse:parse_form(Toks1), {ok, Toks2, _} = selfhost_chain_scan:string("-export([twice/1]).", 1), {ok, F2} = erl_parse:parse_form(Toks2), {ok, Toks3, _} = selfhost_chain_scan:string("twice(N) -> 2 * N.", 1), {ok, F3} = erl_parse:parse_form(Toks3), {ok, chain_scanned, ScannedBin} = compile:forms([F1, F2, F3], [binary, no_spawn_compiler_process]), {module, chain_scanned} = code:load_binary(chain_scanned, "chain_scanned.beam", ScannedBin), {[element(1, T) || T <- Toks3], chain_scanned:twice(21), erlang:function_exported(selfhost_chain_scan, string, 1)} end"#;

/// The **explicit option list** of the forms route: the tuple shapes and the invariant artifact.
///
/// Shape rules, all load-bearing:
///
/// * the option lists are the documented ones: `[binary]`, the explicit
///   `[binary, return_errors, return_warnings, debug_info]`, `[binary, report]` and
///   `compile:noenv_forms(Forms, [binary, return_errors])`. What is pinned is the **shape** of
///   what each returns (`{ok, Mod, Bin}` versus `{ok, Mod, Bin, Warns}`) — classified into
///   `three`/`{four, Warns}` so the golden is a shape rather than a compiler-version tuple
///   ordering — plus the empty warning list of a clean form list;
/// * `report` is exercised only on **clean** forms, where it emits nothing (see the module docs
///   for the measured printing route that is deliberately not pinned), so stdout stays the
///   probe's single line;
/// * the three successful routes must produce the **same `Code` chunk** — the option list is a
///   reporting choice, not an object-code choice — and the `debug_info` image must declare every
///   chunk the loader needs (`AtU8`/`Code`/`StrT`/`ImpT`/`ExpT`), pinned by *membership* so a
///   release that adds a chunk (or `LocT`) cannot drift the golden;
/// * no hash value, no chunk size and no chunk *order* is pinned, and the raise is not caught.
const OPTION_LIST_EXPR: &str = r#"begin Forms = [{attribute, 1, module, chain_opts}, {attribute, 2, export, [{f, 1}]}, {function, 3, f, 1, [{clause, 3, [{var, 3, 'X'}], [], [{var, 3, 'X'}]}]}], Shape = fun(Result) -> case Result of {ok, M, B} when is_atom(M), is_binary(B) -> three; {ok, M, B, W} when is_atom(M), is_binary(B), is_list(W) -> {four, W} end end, {ok, chain_opts, BinPlain} = compile:forms(Forms, [binary]), {ok, chain_opts, Bin, Warns} = compile:forms(Forms, [binary, return_errors, return_warnings, debug_info]), {ok, chain_opts, BinNoenv} = compile:noenv_forms(Forms, [binary, return_errors]), {ok, {chain_opts, [{"Code", CPlain}]}} = beam_lib:chunks(BinPlain, ["Code"]), {ok, {chain_opts, [{"Code", CDebug}]}} = beam_lib:chunks(Bin, ["Code"]), {ok, {chain_opts, [{"Code", CNoenv}]}} = beam_lib:chunks(BinNoenv, ["Code"]), {ok, chain_opts, Chunks} = beam_lib:all_chunks(Bin), Ids = [Id || {Id, _} <- Chunks], {Shape(compile:forms(Forms, [binary])), Shape(compile:forms(Forms, [binary, return_errors, return_warnings])), Shape(compile:forms(Forms, [binary, report])), Shape(compile:noenv_forms(Forms, [binary, return_errors])), Warns, CPlain =:= CDebug, CPlain =:= CNoenv, lists:all(fun(Id) -> lists:member(Id, Ids) end, ["AtU8", "Code", "StrT", "ImpT", "ExpT"])} end"#;

/// The embedded driver of the **three-hop** case: it compiles module A in-process, loads it, and
/// lets A compile, load and call module B.
///
/// Shape rules, all load-bearing:
///
/// * the driver's own source is `chain_stage_a`'s, held **inside** it as a binary: A has to be a
///   source the runtime parses (`epp:parse_file/2`) rather than a form literal, so that the hop
///   the driver performs is the same staged route the expression cases pin;
/// * A is given the case directory (`run(Dir)`) instead of deriving it from `code:which/1`: A is
///   loaded from a `code:load_binary/3` *name*, not from a file, so it has no directory of its
///   own — and the case dir is where B's source is staged;
/// * A stages B's source through the runtime's file-server **write** path, parses it, compiles
///   it with `compile:forms/2`, loads it and calls it, so the chain is
///   case → A (compiled here) → B (compiled by A) → the value (`21 * 2`);
/// * the observable is an **integer**: `42` iff every hop happened, `2` if the driver's own
///   compile did not produce `chain_stage_a`, and the pattern matches (`{ok, Mod, Bin}`,
///   `{module, Mod}`) are left to raise — a runtime that cannot host the route reports its typed
///   gap instead of answering a plausible integer;
/// * every artifact is a distinct module name no installation owns, so the chain cannot be
///   satisfied by a module that was already there.
const TWO_STAGE_SOURCE: &str = r#"-module(selfhost_chain_stage).
-export([run/0]).
%% A.9 three-hop chain: compile module A in-process, load it, then call it — and A compiles,
%% loads and calls module B. `42` iff all four hops happened, `2` if the driver's compile
%% produced no loadable module, and every `= {ok, ...}` / `= {module, ...}` match is left to
%% raise (the route's own failure must escape as the runtime's typed gap).
run() ->
    Dir = filename:dirname(code:which(?MODULE)),
    SourceA = filename:join(Dir, "chain_stage_a.erl"),
    ok = file:write_file(SourceA, a_source()),
    {ok, FormsA} = epp:parse_file(SourceA, []),
    {ok, chain_stage_a, BinA} = compile:forms(FormsA, [binary, no_spawn_compiler_process, deterministic]),
    {module, chain_stage_a} = code:load_binary(chain_stage_a, "chain_stage_a.beam", BinA),
    chain_stage_a:run(Dir).
%% Module A: the second hop. It stages module B's source, parses it, compiles it in-process,
%% loads it and returns B's value — so B is compiled by a module this run compiled.
a_source() ->
    <<"-module(chain_stage_a).\n"
      "-export([run/1]).\n"
      "run(Dir) ->\n"
      "    SourceB = filename:join(Dir, \"chain_stage_b.erl\"),\n"
      "    ok = file:write_file(SourceB, b_source()),\n"
      "    {ok, FormsB} = epp:parse_file(SourceB, []),\n"
      "    {ok, chain_stage_b, BinB} = compile:forms(FormsB, [binary, no_spawn_compiler_process]),\n"
      "    {module, chain_stage_b} = code:load_binary(chain_stage_b, \"chain_stage_b.beam\", BinB),\n"
      "    chain_stage_b:value().\n"
      "b_source() ->\n"
      "    <<\"-module(chain_stage_b).\\n-export([value/0]).\\nvalue() -> 21 * 2.\\n\">>.\n">>.
"#;

/// The embedded driver of the **file-borne documentation** case: a module whose `-moduledoc`
/// names a sibling file, compiled through the *staged* route.
///
/// Shape rules, all load-bearing:
///
/// * the staged module's source is written **as a file** (`selfhost_chain_doc_lib.erl`) and
///   parsed with `epp:parse_file/2`, because the documented route is `epp`'s — `compile:forms/2`
///   is then handed an already-parsed form list, exactly as in the expression cases;
/// * its `-moduledoc({file, "chain_moduledoc.txt"})` attribute makes `epp` read the **sibling
///   file** through the file server (`file:read_file_info/1` + `file:read/2`), which is the
///   io-service `read_handle_info` surface the runtime's file-server floor exists for;
/// * the observable is the produced `Docs` chunk's **module doc**, compared against the text
///   staged here: an unreadable doc file only *warns* and compiles an empty doc, so a bare
///   `{ok, Mod, Bin}` match would pass on a runtime that never served the read. The observable
///   is the staged module's value (`7`) only after the doc text matched, `2` otherwise;
/// * the doc text is plain ASCII with no leading/trailing whitespace, so the compiler's own
///   `string:trim/1` normalisation is an identity and the chunk's value is the file's bytes;
/// * the doc chunk's module-doc field is a bare binary before OTP 27 and a language-keyed map
///   from OTP 27 on, so both shapes are accepted (the golden is the text equality, never a
///   doc-format detail of the oracle's release).
const MODULEDOC_FILE_SOURCE: &str = r#"-module(selfhost_chain_doc).
-export([run/0]).
%% A.9 file-borne documentation through the STAGED route: write the staged module's source and
%% its doc file, parse the source with `epp:parse_file/2` (which must read the doc file through
%% the file server), compile the forms, and check the produced `Docs` chunk carries exactly the
%% staged text before loading the module and calling it. `7` iff both hops happened.
run() ->
    Dir = filename:dirname(code:which(?MODULE)),
    Doc = list_to_binary(doc_text()),
    ok = file:write_file(filename:join(Dir, "chain_moduledoc.txt"), Doc),
    Source = filename:join(Dir, "selfhost_chain_doc_lib.erl"),
    ok = file:write_file(Source, lib_source()),
    {ok, Forms} = epp:parse_file(Source, []),
    {ok, selfhost_chain_doc_lib, Bin} = compile:forms(Forms, [binary, no_spawn_compiler_process, deterministic]),
    {ok, {selfhost_chain_doc_lib, [{"Docs", Docs}]}} = beam_lib:chunks(Bin, ["Docs"]),
    {module, selfhost_chain_doc_lib} = code:load_binary(selfhost_chain_doc_lib, "selfhost_chain_doc_lib.beam", Bin),
    case module_doc(Docs) =:= Doc of
        true -> selfhost_chain_doc_lib:value();
        false -> 2
    end.
%% The documentation this run stages into the sibling file.
doc_text() ->
    "The staged route carries a file-borne module doc.\n"
    "\n"
    "epp reads the sibling file through the file server before compile:forms sees a form.".
%% The staged module: its documentation lives in the sibling file, and it exports one function so
%% the artifact is loadable and callable.
lib_source() ->
    <<"-module(selfhost_chain_doc_lib).\n"
      "-moduledoc({file, \"chain_moduledoc.txt\"}).\n"
      "-export([value/0]).\n"
      "value() -> 7.\n">>.
%% The module doc the produced object code carries, read back from its `Docs` chunk. The chunk is
%% a compressed `docs_v1` term whose module-doc field is a bare binary before OTP 27 and a
%% language-keyed map from OTP 27 on, so both shapes are accepted.
module_doc(Docs) ->
    case binary_to_term(Docs) of
        {docs_v1, _Anno, _Mod, _Format, Doc, _Meta, _Docs} when is_binary(Doc) -> Doc;
        {docs_v1, _Anno, _Mod, _Format, Doc, _Meta, _Docs} when is_map(Doc) ->
            maps:get(<<"en">>, Doc, <<>>)
    end.
"#;

/// The **stage seam**: the explicit `epp` → `erl_lint` → `erl_scan` → `compile:forms/2` chain
/// against `compile:file/2` on the identical source.
fn staged_relation_case() -> Case {
    Case {
        id: "selfhost_chain/epp_erl_lint_erl_scan_then_compile_forms_matches_the_file_route",
        description: "the A.9 stage seam: parse lib/stdlib/src/dets_sup.erl with epp, lint the \
                      forms with erl_lint, tokenize a snippet with erl_scan, compile the forms \
                      with compile:forms/2 and report whether the produced Code chunk, export \
                      table and md5-significant chunk list match the compile:file/2 route's \
                      artifact (the token categories and the source's -behaviour are pinned too)",
        contracts: &[Contract::BeamAsm, Contract::Build],
        kind: CaseKind::Eval {
            expr: STAGED_RELATION_EXPR,
            extra_argv: &[],
            probe: Probe::Value,
        },
        golden: Golden {
            stdout: Some(
                "ok {[],[atom,'(',var,')','->',integer,'*',var,dot],true,true,true,[supervisor]}",
            ),
            exit_code: 0,
        },
    }
}

/// The **compile → load → run** hand-off of the staged route (`erl_internal`).
fn staged_call_case() -> Case {
    Case {
        id: "selfhost_chain/staged_route_loads_and_calls_a_linted_compiler_dependency",
        description: "the A.9 compile-load-call hand-off: parse lib/stdlib/src/erl_internal.erl \
                      with epp, lint it with erl_lint, compile the re-named forms in-process, \
                      check beam_lib:md5/1's 16-byte shape, load the image with \
                      code:load_binary/3 and call eight of the freshly loaded copy's operator / \
                      BIF / type-test predicates",
        contracts: &[Contract::BeamAsm, Contract::Build],
        kind: CaseKind::Eval {
            expr: STAGED_CALL_EXPR,
            extra_argv: &[],
            probe: Probe::Value,
        },
        golden: Golden {
            stdout: Some("ok {[],16,true,false,true,false,true,true,false,true,true}"),
            exit_code: 0,
        },
    }
}

/// **The compiler compiling one of its own passes**: `erl_bifs` compiled, then asked.
fn compiler_pass_case() -> Case {
    Case {
        id: "selfhost_chain/compiler_pass_answers_from_the_freshly_compiled_copy",
        description: "the A.9 compiler-compiling-a-compiler chain: compile \
                      lib/compiler/src/erl_bifs.erl (the purity/safety pass) in-process, load \
                      the fresh copy as selfhost_chain_erl_bifs, and ask IT both polarities of \
                      is_pure/is_safe/is_exit_bif — the answers the compiler's own analysis \
                      gives on this runtime",
        contracts: &[Contract::BeamAsm, Contract::Build],
        kind: CaseKind::Eval {
            expr: COMPILER_PASS_EXPR,
            extra_argv: &[],
            probe: Probe::Value,
        },
        golden: Golden {
            stdout: Some("ok {true,false,true,false,true,false}"),
            exit_code: 0,
        },
    }
}
/// The **fresh scanner drives the next stage** (`erl_scan` compiled, then used to tokenize).
fn compiled_scan_case() -> Case {
    Case {
        id: "selfhost_chain/compiled_erl_scan_scans_the_next_module",
        description: "the A.9 scanner-stage chain: compile lib/stdlib/src/erl_scan.erl (2318 \
                      source lines) in-process, load the fresh copy as selfhost_chain_scan, \
                      tokenize a three-form module's source with it, parse each form with \
                      erl_parse:parse_form/1, compile them with compile:forms/2, load the image \
                      and call the scanned module's function",
        contracts: &[Contract::BeamAsm, Contract::Build],
        kind: CaseKind::Eval {
            expr: COMPILED_SCAN_EXPR,
            extra_argv: &[],
            probe: Probe::Value,
        },
        golden: Golden {
            stdout: Some("ok {[atom,'(',var,')','->',integer,'*',var,dot],42,true}"),
            exit_code: 0,
        },
    }
}

/// The **three-hop** chain: the case compiles A, A compiles B, B's value is the observable.
fn two_stage_case() -> Case {
    Case {
        id: "selfhost_chain/two_stage_compiled_module_compiles_and_runs_the_next",
        description: "the A.9 three-hop chain: compile module chain_stage_a in-process, load it, \
                      then call it — and it stages module chain_stage_b's source, parses it, \
                      compiles it with compile:forms/2, loads it and returns its value, so the \
                      module that compiles B is one this run compiled",
        contracts: &[Contract::BeamAsm, Contract::Build],
        kind: CaseKind::Module(ModuleRun {
            source: TWO_STAGE_SOURCE,
            module: "selfhost_chain_stage",
            function: "run",
            arity: 0,
            args: &[],
            nif: None,
        }),
        golden: Golden {
            stdout: Some("42"),
            exit_code: 0,
        },
    }
}

/// The **file-borne documentation** case on the staged route.
fn moduledoc_file_case() -> Case {
    Case {
        id: "selfhost_chain/moduledoc_from_a_file_through_the_staged_route",
        description: "the A.9 file-borne documentation read through the staged route: stage a \
                      module whose -moduledoc({file, \"chain_moduledoc.txt\"}) names a sibling \
                      file plus that file, parse the source with epp:parse_file/2 (which must \
                      read the doc file through the file server), compile the forms with \
                      compile:forms/2 and report the staged module's value only when the produced \
                      Docs chunk carries exactly the staged text",
        contracts: &[Contract::BeamAsm, Contract::Build],
        kind: CaseKind::Module(ModuleRun {
            source: MODULEDOC_FILE_SOURCE,
            module: "selfhost_chain_doc",
            function: "run",
            arity: 0,
            args: &[],
            nif: None,
        }),
        golden: Golden {
            stdout: Some("7"),
            exit_code: 0,
        },
    }
}

/// The **explicit option list** case: the forms route's tuple shapes and invariant artifact.
fn option_list_case() -> Case {
    Case {
        id: "selfhost_chain/forms_option_list_shapes_and_the_invariant_artifact",
        description: "the A.9 forms-route option surface: [binary] and [binary, report] and \
                      compile:noenv_forms(Forms, [binary, return_errors]) return {ok, Mod, Bin} \
                      while [binary, return_errors, return_warnings, debug_info] returns \
                      {ok, Mod, Bin, Warns} with an empty warning list, all routes producing the \
                      same Code chunk, and the debug_info image declaring the loader's mandatory \
                      chunk set",
        contracts: &[Contract::BeamAsm, Contract::Build],
        kind: CaseKind::Eval {
            expr: OPTION_LIST_EXPR,
            extra_argv: &[],
            probe: Probe::Value,
        },
        golden: Golden {
            stdout: Some("ok {three,{four,[]},three,three,[],true,true,true}"),
            exit_code: 0,
        },
    }
}

/// The A.9 **staged self-hosted chain** group: the hand-off between toolchain stages.
///
/// Seven cases, in the order the module docs introduce them: the stage seam against the
/// `compile:file/2` shortcut, the compile-load-call hand-off of that route, the compiler
/// compiling one of its own passes (`erl_bifs`) and a scanner (`erl_scan`) which are then
/// *used*, the
/// three-hop case→A→B chain, `epp`'s file-borne documentation on the staged route, and the
/// forms route's explicit option list.
pub(crate) fn group() -> Vec<Case> {
    vec![
        staged_relation_case(),
        staged_call_case(),
        compiler_pass_case(),
        compiled_scan_case(),
        two_stage_case(),
        moduledoc_file_case(),
        option_list_case(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::path::Path;

    /// The measured goldens, one per case id: each is the **oracle's** own stdout line,
    /// measured by running the case (`differential run <id>` prints it as `oracle: … stdout=…`).
    const MEASURED: &[(&str, &str)] = &[
        (
            "selfhost_chain/epp_erl_lint_erl_scan_then_compile_forms_matches_the_file_route",
            "ok {[],[atom,'(',var,')','->',integer,'*',var,dot],true,true,true,[supervisor]}",
        ),
        (
            "selfhost_chain/staged_route_loads_and_calls_a_linted_compiler_dependency",
            "ok {[],16,true,false,true,false,true,true,false,true,true}",
        ),
        (
            "selfhost_chain/compiler_pass_answers_from_the_freshly_compiled_copy",
            "ok {true,false,true,false,true,false}",
        ),
        (
            "selfhost_chain/compiled_erl_scan_scans_the_next_module",
            "ok {[atom,'(',var,')','->',integer,'*',var,dot],42,true}",
        ),
        (
            "selfhost_chain/two_stage_compiled_module_compiles_and_runs_the_next",
            "42",
        ),
        (
            "selfhost_chain/moduledoc_from_a_file_through_the_staged_route",
            "7",
        ),
        (
            "selfhost_chain/forms_option_list_shapes_and_the_invariant_artifact",
            "ok {three,{four,[]},three,three,[],true,true,true}",
        ),
    ];

    /// The repo root, as the harness spawns the runtimes (the expression cases address their
    /// sources repo-relatively, so this is the only directory they work from).
    fn repo_root() -> &'static Path {
        Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../../.."))
    }

    /// The expression of a case, panicking when it is not an expression case.
    fn expr_of(case: &Case) -> &'static str {
        let Some(expr) = case.expr() else {
            panic!("case {} is not an expression case", case.id);
        };
        expr
    }

    /// The module-run invocation of a case, panicking when it is not a module-run case.
    fn run_of(case: &Case) -> &ModuleRun {
        let CaseKind::Module(run) = &case.kind else {
            panic!("case {} is not a module-run case", case.id);
        };
        run
    }

    /// The seven cases, in order, each with the contract pair every A.9 case here claims.
    #[test]
    fn the_group_is_the_seven_stage_hand_off_cases_in_order() {
        let cases = group();
        assert_eq!(
            cases.len(),
            7,
            "the group is the stage seam, the compile-load-call hand-off, the compiled linter, \
             the compiled scanner, the three-hop chain, the file-borne documentation and the \
             option-list case"
        );
        for (case, (id, _)) in cases.iter().zip(MEASURED) {
            assert_eq!(case.id, *id, "the group keeps its documented order");
            assert_eq!(
                case.contracts,
                &[Contract::BeamAsm, Contract::Build],
                "case {}: the staged chain produces and runs a real .beam, so it claims A.2 + A.9",
                case.id
            );
            assert_eq!(case.golden.exit_code, 0, "case {}", case.id);
            assert!(
                !case.description.is_empty(),
                "case {} has no description",
                case.id
            );
        }
    }

    /// Every golden is the oracle's own line, and the "allowlist is fresh" rule means a case that
    /// does not agree must not be landed at all — so the table above is the contract of this file.
    #[test]
    fn every_case_carries_its_measured_oracle_golden() {
        let cases = group();
        assert_eq!(cases.len(), MEASURED.len(), "one measured line per case");
        for (case, (id, golden)) in cases.iter().zip(MEASURED) {
            assert_eq!(case.id, *id);
            assert_eq!(
                case.golden.stdout,
                Some(*golden),
                "case {}: the golden must be the oracle's measured line",
                case.id
            );
        }
        // The two module-run cases carry plain integer lines (the module-run printer's shape);
        // every expression case starts with `ok ` (the probe's success line).
        for (case, _) in cases.iter().zip(MEASURED) {
            let golden = case.golden.stdout.expect("every case pins its stdout");
            match case.kind {
                CaseKind::Module(_) => assert!(
                    golden.chars().all(|c| c.is_ascii_digit()),
                    "case {}: a module-run golden is a plain integer line",
                    case.id
                ),
                CaseKind::Eval { probe, .. } => {
                    assert_eq!(probe, Probe::Value, "case {}: a `~w` value", case.id);
                    assert!(
                        golden.starts_with("ok "),
                        "case {}: an expression golden is the probe's success line",
                        case.id
                    );
                }
                _ => panic!("case {}: an unexpected kind", case.id),
            }
        }
    }

    /// The ids are this group's own, unique, and collide with nothing already in the corpus —
    /// in particular not with the twenty-six `selfhost/` ids the sibling group owns.
    #[test]
    fn the_ids_are_unique_and_do_not_collide_with_the_sibling_selfhost_group() {
        let cases = group();
        let mut seen = HashSet::new();
        for case in &cases {
            assert!(
                case.id.starts_with("selfhost_chain/"),
                "case {}: the group's ids are selfhost_chain/<name>",
                case.id
            );
            assert!(
                !case.id.ends_with('/') && case.id.matches('/').count() == 1,
                "case {}: exactly one group separator",
                case.id
            );
            assert!(seen.insert(case.id), "duplicate case id {}", case.id);
        }
        let sibling: HashSet<&str> = super::super::selfhost::group()
            .iter()
            .map(|case| case.id)
            .collect();
        for case in &cases {
            assert!(
                !sibling.contains(case.id),
                "case {}: must not reuse a selfhost/ id",
                case.id
            );
        }
        // The whole registry agrees on uniqueness too (its own well-formedness test runs it);
        // checking here as well keeps a collision local to this file's failure message.
        let registry: HashSet<&str> = super::super::cases().iter().map(|case| case.id).collect();
        for case in &cases {
            assert!(
                registry.contains(case.id),
                "case {}: the id must be reachable through the registry",
                case.id
            );
        }
    }

    /// Every compile route names a **real** `lib/*/src/*.erl` path that exists in the tree (the
    /// expression cases address it repo-relatively, so a moved source would silently turn into a
    /// `{badmatch, {error, enoent}}` on both sides instead of a visible failure), and every
    /// staged subject is a source the route can actually reach.
    #[test]
    fn every_compile_route_names_a_source_that_exists_in_the_tree() {
        let cases = group();
        for (index, source) in [
            (0usize, "lib/stdlib/src/dets_sup.erl"),
            (1, "lib/stdlib/src/erl_internal.erl"),
            (2, "lib/compiler/src/erl_bifs.erl"),
            (3, "lib/stdlib/src/erl_scan.erl"),
        ] {
            let expr = expr_of(&cases[index]);
            let needle = format!("epp:parse_file(\"{source}\"");
            assert!(
                expr.contains(&needle),
                "case {}: the staged route parses the repo-relative {source:?}",
                cases[index].id
            );
            let path = repo_root().join(source);
            assert!(
                path.is_file(),
                "case {}: {source:?} must exist in the tree ({})",
                cases[index].id,
                path.display()
            );
        }
        // The module-run cases write their staged sources into the case dir, so their subjects
        // must be *distinct* module names: a duplicate name would let the harness's own compile
        // satisfy a route that never compiled anything.
        let mut staged = HashSet::new();
        for needle in [
            "-module(chain_stage_a)",
            "-module(chain_stage_b)",
            "-module(selfhost_chain_doc_lib)",
        ] {
            assert!(staged.insert(needle), "distinct staged module {needle}");
        }
        assert!(
            run_of(&cases[4]).source.contains("-module(chain_stage_a)."),
            "the three-hop case stages module A"
        );
        assert!(
            run_of(&cases[4]).source.contains("-module(chain_stage_b)"),
            "and module A stages module B"
        );
    }

    /// Whether `name` occurs in `text` as a whole token (not as part of a longer identifier).
    fn occurs_as_token(text: &str, name: &str) -> bool {
        let is_ident = |c: Option<char>| c.is_some_and(|c| c.is_alphanumeric() || c == '_');
        text.match_indices(name).any(|(at, _)| {
            let before = text[..at].chars().next_back();
            let after = text[at + name.len()..].chars().next();
            !is_ident(before) && !is_ident(after)
        })
    }

    /// No expression may use `V`, `C` or `R` — the probe wrapper binds them, so an expression
    /// that reuses one of those names fails to compile *on the oracle* (a guaranteed false
    /// mismatch) — and none may catch the raise, which must escape as the runtime's typed gap.
    #[test]
    fn no_expression_shadows_a_probe_binding_or_catches_the_raise() {
        for case in group() {
            let text = match case.kind {
                CaseKind::Eval { expr, .. } => expr,
                CaseKind::Module(run) => run.source,
                _ => panic!("case {}: an unexpected kind", case.id),
            };
            for binding in ["V", "C", "R"] {
                assert!(
                    !occurs_as_token(text, binding),
                    "case {}: the probe wrapper already binds {binding}",
                    case.id
                );
            }
            assert!(
                !text.contains("catch"),
                "case {}: the raise must not be caught",
                case.id
            );
            assert!(
                !text.contains("erlang:md5") && !text.contains("phash2"),
                "case {}: no compiler-release hash may reach the golden",
                case.id
            );
        }
    }

    /// The **stage seam** case really is the explicit staged route, and its relation is the
    /// artifact equality (never a hash value): the three stages are invoked by name, the
    /// comparison is against `compile:file/2` on the identical source, and the md5-significant
    /// chunk list is the one `beam_lib` uses *minus* `Meta` (the chunk that makes the two images'
    /// `beam_lib:md5/1` differ — see the module docs).
    #[test]
    fn the_stage_seam_case_compares_the_staged_forms_against_the_file_route() {
        let cases = group();
        let expr = expr_of(&cases[0]);
        for needle in [
            "epp:parse_file(\"lib/stdlib/src/dets_sup.erl\", [])",
            "erl_lint:module(Forms)",
            "erl_scan:string(\"twice(N) -> 2 * N.\")",
            "compile:forms(Forms, [binary, no_spawn_compiler_process, deterministic])",
            "compile:file(File, [binary, no_spawn_compiler_process, deterministic])",
            "beam_lib:chunks(Bin, [\"Code\"])",
            "beam_lib:chunks(FileBin, [\"Code\"])",
            "Code =:= FileCode",
            "Exports =:= FileExports",
            "Sig(Bin) =:= Sig(FileBin)",
            "proplists:get_value(behaviour, Attrs)",
        ] {
            assert!(expr.contains(needle), "the relation needs {needle:?}");
        }
        // The relation is on the md5-significant chunks **minus** `Meta`: `Meta` is the chunk
        // whose presence in the file route's image (and absence from the forms route's) makes
        // `beam_lib:md5/1` differ between the two routes on OTP 29 — a compiler property, not a
        // runtime one, so it must not be pinned.
        assert!(
            !expr.contains("\"Meta\""),
            "Meta is deliberately excluded from the compared chunk list"
        );
        for id in [
            "\"Atom\"", "\"AtU8\"", "\"Code\"", "\"StrT\"", "\"ImpT\"", "\"ExpT\"", "\"FunT\"",
            "\"LitT\"", "\"Recs\"", "\"DbgB\"",
        ] {
            assert!(
                expr.contains(id),
                "the compared md5-significant chunk list needs {id}"
            );
        }
        // Only the two routes' `Code` chunks are compared with each other: no artifact image is
        // compared against a literal, so no compiler-release image can reach the golden.
        assert!(!expr.contains("(Code =:= <<"), "the Code chunk is never pinned");
    }

    /// The **compile → load → run** case really loads what it compiled: the module attribute is
    /// renamed (so the artifact is loadable at all), the image is loaded with
    /// `code:load_binary/3` *before* the calls, and the calls go to the renamed module.
    #[test]
    fn the_staged_call_case_loads_the_artifact_before_it_calls_it() {
        let cases = group();
        let expr = expr_of(&cases[1]);
        for needle in [
            "epp:parse_file(\"lib/stdlib/src/erl_internal.erl\", [])",
            "{attribute, Anno, module, selfhost_chain_internal}",
            "erl_lint:module(Forms)",
            "beam_lib:md5(Bin)",
            "code:load_binary(selfhost_chain_internal, \"selfhost_chain_internal.beam\", Bin)",
        ] {
            assert!(expr.contains(needle), "the route needs {needle:?}");
        }
        let load = expr.find("code:load_binary").expect("the load");
        let first_call = expr
            .find("selfhost_chain_internal:arith_op")
            .expect("the calls");
        assert!(
            load < first_call,
            "the artifact is loaded before the freshly compiled code is called"
        );
        // The original module name must not survive as a call target: a call to `erl_internal:`
        // would exercise the installation's module, not the one this run produced.
        assert!(
            !expr.contains("erl_internal:arith_op")
                && !expr.contains("erl_internal:bif")
                && !expr.contains("erl_internal:bool_op"),
            "every call goes to the renamed, freshly loaded module"
        );
        // Eight predicates, both polarities, so a runtime that loaded the wrong module (or none)
        // cannot answer the golden.
        assert_eq!(
            expr.matches("selfhost_chain_internal:").count(),
            8,
            "the case calls eight of the freshly loaded copy's predicates"
        );
        assert!(
            expr.contains("selfhost_chain_internal:arith_op('foo', 2)")
                && expr.contains("selfhost_chain_internal:bif(nonsense, 3)"),
            "the negative answers are part of the observable"
        );
    }

    /// The two **compiler-compiling-a-compiler** cases compile a real toolchain stage, load the
    /// fresh copy under a name the installation does not own, and then *use* it — the linter to
    /// lint, the scanner to scan — before the rest of the chain (parse/compile/load/call) runs on
    /// the fresh copy's output.
    #[test]
    fn the_compiled_stages_are_loaded_and_then_used() {
        let cases = group();
        let pass = expr_of(&cases[2]);
        for needle in [
            "epp:parse_file(\"lib/compiler/src/erl_bifs.erl\", [])",
            "{attribute, A, module, selfhost_chain_erl_bifs}",
            "code:load_binary(selfhost_chain_erl_bifs, \"selfhost_chain_erl_bifs.beam\", Bin)",
            "selfhost_chain_erl_bifs:is_pure(erlang, abs, 1)",
            "selfhost_chain_erl_bifs:is_pure(erlang, put, 2)",
            "selfhost_chain_erl_bifs:is_safe(erlang, is_atom, 1)",
            "selfhost_chain_erl_bifs:is_safe(erlang, put, 2)",
            "selfhost_chain_erl_bifs:is_exit_bif(erlang, exit, 1)",
            "selfhost_chain_erl_bifs:is_exit_bif(erlang, abs, 1)",
        ] {
            assert!(pass.contains(needle), "the compiler-pass chain needs {needle:?}");
        }
        let load = pass.find("code:load_binary").expect("the load");
        let first_ask = pass
            .find("selfhost_chain_erl_bifs:is_pure")
            .expect("the purity questions");
        assert!(
            load < first_ask,
            "the freshly compiled pass is loaded before it is asked"
        );

        let scan = expr_of(&cases[3]);
        for needle in [
            "epp:parse_file(\"lib/stdlib/src/erl_scan.erl\", [])",
            "{attribute, A, module, selfhost_chain_scan}",
            "code:load_binary(selfhost_chain_scan, \"selfhost_chain_scan.beam\", Bin)",
            "selfhost_chain_scan:string(\"-module(chain_scanned).\", 1)",
            "erl_parse:parse_form(Toks1)",
            "compile:forms([F1, F2, F3], [binary, no_spawn_compiler_process])",
            "code:load_binary(chain_scanned, \"chain_scanned.beam\", ScannedBin)",
            "chain_scanned:twice(21)",
        ] {
            assert!(scan.contains(needle), "the scanner chain needs {needle:?}");
        }
        // The fresh scanner's tokens are what the rest of the chain consumes: the same three
        // token lists the three forms are parsed from, and no `epp` on the scanned source (which
        // would bypass the fresh stage entirely).
        assert_eq!(
            scan.matches("selfhost_chain_scan:string(").count(),
            3,
            "each of the three forms is tokenized by the fresh scanner"
        );
        assert!(
            !scan.contains("epp:parse_file(\"chain_scanned"),
            "the scanned module is parsed from the fresh scanner's tokens, not through epp"
        );
        assert_eq!(
            scan.matches("code:load_binary").count(),
            2,
            "both the fresh scanner and the scanned module are loaded"
        );
    }

    /// The **three-hop** and **file-borne documentation** cases: the staged module is written
    /// through the runtime's own write path *before* the compile that reads it, the loaded module
    /// is the one this run compiled, and the observable is an integer with an explicit
    /// not-matched fallback.
    #[test]
    fn the_module_run_cases_stage_their_sources_before_compiling_them() {
        let cases = group();
        let stage = run_of(&cases[4]);
        assert_eq!(stage.module, "selfhost_chain_stage");
        assert_eq!(stage.function, "run");
        assert_eq!(stage.arity as usize, stage.args.len());
        assert!(stage.nif.is_none(), "the routes need no C/NIF fixture");
        for needle in [
            "file:write_file(SourceA, a_source())",
            "epp:parse_file(SourceA, [])",
            "compile:forms(FormsA, [binary, no_spawn_compiler_process, deterministic])",
            "{module, chain_stage_a} = code:load_binary(chain_stage_a",
            "chain_stage_a:run(Dir)",
        ] {
            assert!(stage.source.contains(needle), "the chain needs {needle:?}");
        }
        let write = stage.source.find("file:write_file(SourceA").expect("write");
        let parse = stage.source.find("epp:parse_file(SourceA").expect("parse");
        let load = stage
            .source
            .find("code:load_binary(chain_stage_a")
            .expect("load");
        let call = stage.source.find("chain_stage_a:run(Dir)").expect("call");
        assert!(
            write < parse && parse < load && load < call,
            "the driver writes, parses, loads and only then calls the module it compiled"
        );
        // Module A performs the same hand-off for module B, so the chain is genuinely three hops.
        for needle in [
            "file:write_file(SourceB, b_source())",
            "epp:parse_file(SourceB, [])",
            "compile:forms(FormsB, [binary, no_spawn_compiler_process])",
            "{module, chain_stage_b} = code:load_binary(chain_stage_b",
            "chain_stage_b:value()",
        ] {
            assert!(
                stage.source.contains(needle),
                "module A's hop needs {needle:?}"
            );
        }
        assert_eq!(
            stage.source.matches("compile:forms(").count(),
            2,
            "two compiles: the driver's and module A's"
        );

        let doc = run_of(&cases[5]);
        assert_eq!(doc.module, "selfhost_chain_doc");
        assert_eq!(doc.function, "run");
        for needle in [
            "file:write_file(filename:join(Dir, \"chain_moduledoc.txt\"), Doc)",
            "file:write_file(Source, lib_source())",
            "epp:parse_file(Source, [])",
            "-moduledoc({file, \\\"chain_moduledoc.txt\\\"})",
            "beam_lib:chunks(Bin, [\"Docs\"])",
            "module_doc(Docs) =:= Doc",
            "selfhost_chain_doc_lib:value()",
        ] {
            assert!(doc.source.contains(needle), "the doc case needs {needle:?}");
        }
        let write_doc = doc
            .source
            .find("file:write_file(filename:join(Dir, \"chain_moduledoc.txt\")")
            .expect("the doc write");
        let parse = doc.source.find("epp:parse_file(Source").expect("parse");
        assert!(
            write_doc < parse,
            "the doc file is staged before epp reads it"
        );
        // The observable is the doc-text equality, never the compile's return value: an
        // unreadable doc file only warns, so a `{ok, Mod, Bin}` match alone would pass on a
        // runtime that never served the read.
        assert!(
            doc.source.contains("false -> 2"),
            "a doc mismatch must answer a distinct integer"
        );
    }

    /// The **option list** case pins the tuple shapes (classified, not the raw tuple) and the
    /// invariant artifact — and its `report` clause only on clean forms, so stdout stays the
    /// probe's single line.
    #[test]
    fn the_option_list_case_pins_the_tuple_shapes_and_the_invariant_artifact() {
        let cases = group();
        let expr = expr_of(&cases[6]);
        for needle in [
            "compile:forms(Forms, [binary])",
            "compile:forms(Forms, [binary, return_errors, return_warnings, debug_info])",
            "compile:forms(Forms, [binary, return_errors, return_warnings])",
            "compile:forms(Forms, [binary, report])",
            "compile:noenv_forms(Forms, [binary, return_errors])",
            "CPlain =:= CDebug",
            "CPlain =:= CNoenv",
            "[\"AtU8\", \"Code\", \"StrT\", \"ImpT\", \"ExpT\"]",
        ] {
            assert!(expr.contains(needle), "the case needs {needle:?}");
        }
        // The shapes are classified: a raw tuple ordering would pin the compiler's own
        // representation of the warning list.
        assert!(
            expr.contains("-> three;") && expr.contains("{four, W}"),
            "the return shapes are classified, not pinned as tuples"
        );
        // `report` is exercised on CLEAN forms only (the form list has one exported function and
        // no unused ones), because `report` prints a diagnostic to stdout for a warnable module.
        assert!(
            !expr.contains("unused"),
            "a warnable form would make `report` print to stdout"
        );
        assert!(
            !expr.contains("{outdir"),
            "an expression case writes no artifact"
        );
    }
}
