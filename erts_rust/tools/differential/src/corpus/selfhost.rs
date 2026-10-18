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

//! Contracts **A.9 + A.2** — the self-hosted build's **in-process** `compile:file/2`: the
//! **E0 route** (`no_spawn_compiler_process`), the **default, scheduled-process** shape
//! (the `erl -noshell -s compile …` / `erlc` shape), a **real stdlib module**'s object code,
//! a **dependent pair** compiled and linked in one run, a module whose documentation lives in
//! a **sibling file**, the **code-path / load-state** shapes the route stands on, and a
//! **large real module compiled through the expression route**
//! ([`timer_case`] — `lib/stdlib/src/timer.erl`, 1017 source lines).
//!
//! `../../../CONTEXT.md` (its "A.9 — what an in-process compile actually needs" section) is
//! the design source: OTP's `compile:file/2` path reaches only `kernel` + `stdlib` (`epp` — an
//! io-device-only client — plus `erl_features`, `erl_scan`/`erl_parse`/`erl_lint`/
//! `erl_expand_records`, the `v3_core` → `beam_*` passes and `save_binary`'s write). The E0
//! route keeps the whole compile in the **calling** process; the default shape spawns
//! `compile:file/2`'s compiler process, and the caller parks in the receive that waits for it.
//!
//! This group is the **evidence scaffold** for that route: six module-run cases whose goldens
//! are the oracle's own integer lines, plus seven **expression** cases — the large-real-module
//! compile ([`timer_case`]) and six that pin a *relation* of the file-free (`[binary]`) route
//! (two-run determinism, the artifact-versus-source attributes/chunk set, a sibling header, the
//! two routes against each other, an extra include directory, and `-include_lib`). Each pins a
//! property of the *produced object code* rather than a mere "it returned", and no case pins a
//! compiler-release artifact (see *Why the goldens are comparisons and classified shapes*).
//!
//! # The cases
//!
//! Each embedded module self-locates its temp dir (`filename:dirname(code:which(?MODULE))`),
//! stages what it compiles through the runtime's file-server **write** path, and lets the
//! compiler write the object code into the case's outdir — which is the same directory the
//! compiled module was loaded from.
//!
//! * [`case`] — the E0 route (`no_spawn_compiler_process`): it reports the integer `1` once
//!   the produced `<outdir>/selfhost.beam` reads back as a non-empty binary, and a distinct
//!   integer otherwise.
//! * [`sched_case`] — the default shape: it compiles the staged source **twice** with
//!   `deterministic` and reports `1` iff the two produced `.beam`s' `Code` chunks (read back
//!   with `beam_lib:chunks/2`) are byte-identical, a distinct integer otherwise. That equality
//!   is the documented `deterministic` acceptance criterion, and unlike a bytecode hash it is
//!   independent of the compiler release and the host, so the oracle reproduces it unchanged on
//!   any OTP release.
//! * [`stdlib_case`] — the **real stdlib module** case: the embedded source is a verbatim copy
//!   of `lib/stdlib/src/ordsets.erl` (see [`SELFHOST_STDLIB_SOURCE`]), compiled in-process with
//!   the E0 options and compared **byte-for-byte at the object-code level** against the `.beam`
//!   the **system compiler** produced for the identical source — the `erlc` artifact the harness
//!   already compiled into the case dir (or, once the oracle side has run, the oracle's own
//!   in-process artifact). It reports `1` iff the two `Code` chunks are byte-identical *and* the
//!   produced artifact carries no occurrence of the case directory (the `deterministic`
//!   stripping that makes it host-independent), a distinct integer otherwise.
//! * [`pair_case`] — the **dependent pair** case: two sources (`selfhost_pair_a`, and
//!   `selfhost_pair_b` whose `twice/1` calls `selfhost_pair_a:add/2`) are staged and compiled
//!   **in one in-process run, dependency order**, then put on the code path with
//!   `code:add_patha/1`, loaded with `code:load_file/1`, and *called through* — so `1` asserts
//!   the pair really links and runs, not merely that two compiles returned `{ok, _}`.
//! * [`path_case`] — the **code-path / load-state** case: `code:add_patha/1` puts the case dir
//!   at the head of `code:get_path/0`, and the loaded-module surface (`code:ensure_loaded/1`,
//!   `code:is_loaded/1`, `code:all_loaded/1`, `init:fetch_loaded/0`) reports the shapes the
//!   compile-and-load route consumes. As in every sibling boot case, the load is made
//!   **explicit** first (`code:ensure_loaded(lists)`), because this runtime's boot set is
//!   smaller than the oracle's.
//! * [`docfile_case`] — the **file-borne documentation** case: the module's own source carries
//!   `-moduledoc({file, "moduledoc.txt"})`, so compiling it in-process forces `epp` to read the
//!   **sibling doc file** through the file server (`epp`'s `scan_filedoc_content/5` does a
//!   `file:read_file_info/1` + `file:read/2` — the io-service `read_handle_info` request) while
//!   the rest of the compile runs in the calling process. The doc file is staged through the
//!   runtime's own write path first, and `1` asserts that the produced object code's `Docs`
//!   chunk carries **exactly that text** as the module doc. The content check is load-bearing:
//!   an *unreadable* doc file only **warns** and yields an empty module doc (so a bare
//!   `{ok, Mod}` match would happily pass on a runtime that never served the read).
//! * [`timer_case`] — the **large real module** case, and the only **expression** case of the
//!   group: it compiles the repository's own `lib/stdlib/src/timer.erl` (1017 source lines) with
//!   `[binary, no_spawn_compiler_process, deterministic]` — the source is addressed
//!   **repo-relatively**, because the harness spawns both runtimes with the repository root as
//!   the working directory — and pins the produced object image's **export table**. The route is
//!   the expression one on cost (see [`SELFHOST_TIMER_EXPR`]): the same compile driven through
//!   the module-run `run` entry point costs ≈ 43 s on the Rust runtime against ≈ 2.3 s through
//!   `-eval`.
//! * [`two_run_case`] — the **reproducibility** relation: `lib/stdlib/src/dets_sup.erl` (42
//!   source lines) is compiled **twice** with the file-free form, and the two artifacts must
//!   agree byte-for-byte, chunk-for-chunk and md5-image-for-md5-image, carry no source path,
//!   and declare exactly the source's export table.
//! * [`attributes_case`] — the **artifact-versus-source** relation: the produced image's
//!   `attributes` chunk carries the source's `-behaviour(supervisor)`, and `beam_lib:info/1`
//!   declares every chunk a loadable `.beam` needs. The compiler-generated `vsn` entry is pinned
//!   by its presence only.
//! * [`sibling_header_case`] — the **sibling header**: `lib/ssh/src/ssh_bits.erl` carries
//!   `-include("ssh.hrl")`, which `epp` reads from the source's own directory through the file
//!   server; a missing include is a hard compile error, so success proves the read.
//! * [`two_route_case`] — **route independence**: the E0 route and `compile:file/2`'s own
//!   compiler process produce the *same* `Code` chunk and export table from the same source, so
//!   `no_spawn_compiler_process` changes only *where* the work runs.
//! * [`include_dir_case`] — the **extra include directory**: `{i, "lib/xmerl/include"}` is what
//!   makes `xmerl_xpath_lib`'s out-of-directory header reachable, so the option is load-bearing
//!   and the produced export table is the source's.
//! * [`include_lib_case`] — **`-include_lib`**: `ftp_progress`'s
//!   `-include_lib("kernel/include/file.hrl")` is located through the installation's own code
//!   path (`code:lib_dir/1`), the resolution the self-hosted build stands on.
//!
//! In every case the compile's own failure is deliberately **not** caught: a runtime that
//! cannot host the route must raise its typed capability gap (the CLI then exits `3`), never a
//! plausible-looking integer.
//!
//! # Why the goldens are comparisons and classified shapes, never hashes
//!
//! A raw `erlang:md5/1` of a produced `.beam` (or its byte length) is a **compiler-release**
//! artifact: the same source compiled by two OTP releases yields different object code. This
//! corpus' oracle is the system `erl` on `$PATH` — a *different* release in CI (OTP 28) than
//! on a developer host (OTP 29) — so a pinned hash would be corpus drift, i.e. the oracle
//! would stop reproducing its own golden. Every observable here is instead a property that
//! holds on **any** host and release: an equality between two artifacts the same host
//! produced (`.beam` `Code` chunks), a shape classified into an integer, or — for
//! [`timer_case`], which has no `erlc` reference to compare against at an acceptable cost — a
//! **source-derived projection of the produced object code** (its export table, which
//! `-export` attributes plus the compiler's `module_info/0,1` determine), read back out of the
//! produced `.beam` with `beam_lib:chunks(…, [exports])`.
//!
//! The stronger, *cross-runtime* claim is still made — by the harness, not by a hash: the
//! oracle side of [`stdlib_case`] compares its own in-process artifact against a
//! **system-compiler** artifact, and the Rust side compares *its* artifact against the very
//! same file; the case framework then requires the two integer observables to be equal. What
//! is pinned is therefore "the runtime's produced object code equals the object code the
//! system compiler produced from the identical source", computed consistently inside each
//! runtime.
//!
//! # Status
//!
//! All thirteen cases **agree** on the Rust runtime: the runtime hosts the E0 route, the
//! scheduled shape, an in-process compile of a real stdlib module, the dependent pair (with
//! the `code:add_patha/1` + `code:load_file/1` link round trip), a module whose documentation
//! the compiler reads from a sibling file, the code-path / load-state shapes, the
//! in-process compile of `lib/stdlib/src/timer.erl` (1017 lines), and the six file-free
//! relation cases (two-run determinism, the artifact's attributes and chunk set, a sibling
//! header, route independence, an extra include directory, `-include_lib`). Three
//! contracts are in play and exactly two are claimed:
//! [`Contract::BeamAsm`] (a real `.beam` executes and object code is produced) and
//! [`Contract::Build`] (the build/bootstrapping route itself). [`Contract::OtpApps`] is
//! **not** claimed — the routes' whole point is that they reach only `kernel` + `stdlib`, so
//! nothing about them depends on the wider application surface.
//!
//! # Measured divergences / not covered
//!
//! Two routes belong to this contract but are deliberately **not** corpus cases. Each is a
//! ledger line, not a golden:
//!
//! * **Loading a freshly compiled artifact on the expression route** (compile → load → call,
//!   which would compare a compiled module's answers with the same function's in a system
//!   module). The oracle refuses `code:load_binary/3` for a *sticky* stdlib module name with
//!   `{error,sticky_directory}` and needs `code:delete/1` + `code:purge/1` first — and the Rust
//!   runtime answers `unsupported: erlang:delete_module/1` there (owner **`beam_rt`**), the
//!   raise even taking the `code_server` down with it. So the compile-and-**call** dimension
//!   stays on the module-run route, where [`pair_case`] already pins it through
//!   `code:add_patha/1` + `code:load_file/1`.
//! * **The `binary` form against the `{outdir, Dir}` (write-path) form** — the two compile
//!   entry points' artifacts compared with each other. That comparison needs an outdir, hence
//!   the module-run argv, and the very same in-process compile costs **≈ 14 s** on the Rust
//!   runtime there against **≈ 0.34 s** on the oracle (measured while drafting this group),
//!   which a gate that boots both runtimes for every CLI case cannot carry. The outdir form is
//!   still pinned against *itself* by [`sched_case`] (two `deterministic` outdir compiles), and
//!   the file-free form's own reproducibility by [`two_run_case`].
use super::{Case, CaseKind, Contract, Golden, ModuleRun, Probe};

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
/// * its observable is the produced object code itself, compared against itself: the source is
///   compiled **twice** and the two `.beam`s' `Code` chunks (read back through
///   `beam_lib:chunks/2` before the second compile overwrites the first) must be
///   byte-identical. `deterministic` is exactly the guarantee that they are, so the integer (a
///   plain `1`) is release- and host-independent — no bytecode hash, so no compiler-version
///   drift — while still asserting a real `.beam`-parity property once the host runs the
///   compiler, rather than a mere "it returned".
const SELFHOST_SCHED_SOURCE: &str = r#"-module(selfhost_sched).
-export([compile_self/0]).

%% The DEFAULT (contract A.9) route: recompile this module's own source WITHOUT
%% `no_spawn_compiler_process`, so `compile:file/2` spawns its compiler process while this one
%% parks in the receive that waits for it — the `erl -noshell -s compile …` / `erlc` shape.
%% `{outdir, Dir}` makes its last pass (`save_binary`) write `<outdir>/selfhost_sched.beam`
%% through the file server. The observable is the `deterministic` guarantee itself: two
%% compilations of the staged source produce byte-identical `Code` chunks (1), else a distinct
%% integer. No bytecode hash is reported, so the golden never depends on the compiler release.
compile_self() ->
    Dir = filename:dirname(code:which(?MODULE)),
    Source = filename:join(Dir, "selfhost_sched.erl"),
    Staged = filename:join(Dir, "selfhost_sched.staged.erl"),
    {ok, Src} = file:read_file(Source, [raw]),
    ok = file:write_file(Staged, Src),
    First = compile_code(Source, Dir),
    Second = compile_code(Source, Dir),
    case First =:= Second of
        true -> 1;
        false -> 2
    end.

%% One scheduled compile: `compile:file/2`'s own compiler process writes
%% `<outdir>/selfhost_sched.beam` through the file server, and the produced `.beam`'s `Code`
%% chunk is read back before the next compile overwrites it.
compile_code(Source, Dir) ->
    {ok, _Mod} = compile:file(Source, [deterministic, {outdir, Dir}]),
    Beam = filename:join(Dir, "selfhost_sched.beam"),
    {ok, {?MODULE, [{"Code", Code}]}} = beam_lib:chunks(Beam, ["Code"]),
    Code.
"#;

/// The embedded module of the **real stdlib module** case: a verbatim copy of
/// `lib/stdlib/src/ordsets.erl` plus the runner below.
///
/// Two edits, both forced by the module-run shape, and nothing else:
///
/// * the `-module` attribute is renamed `ordsets` → `selfhost_stdlib` (and `compile_self/0` is
///   exported alongside the module's own exports). The rename is load-bearing: a real stdlib
///   module name resolves to the **installation's** own copy, so `code:which(?MODULE)` would
///   name the installed `ebin` instead of this case's directory; and the harness compiles the
///   embedded source under the module name the case declares. The code between the attributes
///   and the runner is **byte-for-byte the stdlib source** — `ordsets`' 30 functions, its
///   `-type`/`-spec`s, its `-doc`/`-moduledoc` attributes — so the compile exercises the real
///   path (parse, lint, expand records, `v3_core` → `beam_ssa` passes, `save_binary`).
/// * the runner tail (`compile_self/0` plus its `code_chunk/1` helper) appended after it.
///
/// The compile itself is the E0 route (`no_spawn_compiler_process`, `deterministic`,
/// `{outdir, Dir}`), and the observable is the produced artifact's **`Code` chunk compared
/// against the object code the system compiler produced for the identical source** (see
/// [`stdlib_case`]).
const SELFHOST_STDLIB_SOURCE: &str = r#"-module(selfhost_stdlib).
-moduledoc """
Functions for manipulating sets as ordered lists.

Sets are collections of elements with no duplicate elements. An `ordset` is a
representation of a set, where an ordered list is used to store the elements of
the set. An ordered list is more efficient than an unordered list. Elements are
ordered according to the _Erlang term order_.

This module provides the same interface as the `m:sets` module but with a
defined representation. One difference is that while `sets` considers two
elements as different if they do not match (`=:=`), this module considers two
elements as different if and only if they do not compare equal (`==`).

See the [Compatibility Section in the `sets` module](`m:sets#module-compatibility`)
for more information about the compatibility of the different implementations of
sets in the Standard Library.

### See Also

`m:gb_sets`, `m:sets`
""".

-export([new/0,is_set/1,size/1,is_empty/1,to_list/1,from_list/1]).
-export([is_element/2,add_element/2,del_element/2]).
-export([union/2,union/1,intersection/2,intersection/1]).
-export([is_equal/2, is_disjoint/2]).
-export([subtract/2,is_subset/2]).
-export([fold/3,filter/2,map/2,filtermap/2]).

-export_type([ordset/1]).
-export([compile_self/0]).

-doc "As returned by `new/0`.".
-type ordset(T) :: [T].

-doc """
Returns a new empty ordered set.

## Examples

```erlang
1> ordsets:new().
[]
```
""".
-spec new() -> ordset(none()).

new() -> [].

-doc """
Returns `true` if `Ordset` is an ordered set of elements; otherwise,
returns `false`.

> #### Note {: .info }
>
> This function returns true for any ordered list, even if it was not
> constructed using the functions in this module.

## Examples

```erlang
1> ordsets:is_set(ordsets:from_list([a,x,13,{p,q}])).
true
2> ordsets:is_set([a,b,c]).
true
3> ordsets:is_set([z,a]).
false
4> ordsets:is_set({a,b}).
false
```
""".
-spec is_set(Ordset) -> boolean() when
      Ordset :: term().

is_set([E|Es]) -> is_set(Es, E);
is_set([]) -> true;
is_set(_) -> false.

is_set([E2|Es], E1) when E1 < E2 ->
    is_set(Es, E2);
is_set([_|_], _) -> false;
is_set([], _) -> true.

-doc """
Returns the number of elements in `Ordset`.

## Examples

```erlang
1> ordsets:size(ordsets:new()).
0
2> ordsets:size(ordsets:from_list([4,5,6])).
3
```
""".
-spec size(Ordset) -> non_neg_integer() when
      Ordset :: ordset(_).

size(S) -> length(S).

-doc """
Returns `true` if `Ordset` is an empty set; otherwise, returns `false`.

## Examples

```erlang
1> ordsets:is_empty(ordsets:new()).
true
2> ordsets:is_empty(ordsets:from_list([1])).
false
```
""".
-doc(#{since => <<"OTP 21.0">>}).
-spec is_empty(Ordset) -> boolean() when
      Ordset :: ordset(_).

is_empty(S) -> S =:= [].

-doc """
Returns `true` if `Ordset1` and `Ordset2` are equal, that is, if every element
of one set is also a member of the other set; otherwise, returns `false`.

## Examples

```erlang
1> Empty = ordsets:new().
2> S = ordsets:from_list([a,b]).
3> ordsets:is_equal(S, S).
true
4> ordsets:is_equal(S, Empty).
false
```
""".
-doc(#{since => <<"OTP 27.0">>}).
-spec is_equal(Ordset1, Ordset2) -> boolean() when
      Ordset1 :: ordset(_),
      Ordset2 :: ordset(_).

is_equal(S1, S2) when is_list(S1), is_list(S2) ->
    S1 == S2.

-doc """
Returns the elements of `Ordset` as a list.

## Examples

```erlang
1> S = ordsets:from_list([a,b]).
2> ordsets:to_list(S).
[a,b]
```
""".
-spec to_list(Ordset) -> List when
      Ordset :: ordset(T),
      List :: [T].

to_list(S) -> S.

-doc """
Returns an ordered set of the elements in `List`.

## Examples

```erlang
1> ordsets:from_list([a,b,a,b,b,c]).
[a,b,c]
```
""".
-spec from_list(List) -> Ordset when
      List :: [T],
      Ordset :: ordset(T).

from_list(L) ->
    lists:usort(L).

-doc """
Returns `true` if `Element` is an element of `Ordset`; otherwise, returns `false`.

## Examples

```erlang
1> S = ordsets:from_list([a,b,c]).
2> ordsets:is_element(42, S).
false
3> ordsets:is_element(b, S).
true
```
""".
-spec is_element(Element, Ordset) -> boolean() when
      Element :: term(),
      Ordset :: ordset(_).

is_element(E, [H|Es]) when E > H -> is_element(E, Es);
is_element(E, [H|_]) when E < H -> false;
is_element(_E, [_H|_]) -> true;			%E == H
is_element(_, []) -> false.

-doc """
Returns a new ordered set formed from `Ordset1` with `Element` inserted.

## Examples

```erlang
1> S0 = ordsets:new().
[]
2> S1 = ordsets:add_element(7, S0).
[7]
3> S2 = ordsets:add_element(42, S1).
[7,42]
4> ordsets:add_element(42, S2).
[7,42]
```
""".
-spec add_element(Element, Ordset1) -> Ordset2 when
      Element :: E,
      Ordset1 :: ordset(T),
      Ordset2 :: ordset(T | E).

add_element(E, [H|Es]) when E > H -> [H|add_element(E, Es)];
add_element(E, [H|_]=Set) when E < H -> [E|Set];
add_element(_E, [_H|_]=Set) -> Set;		%E == H
add_element(E, []) -> [E].

-doc """
Returns a copy of `Ordset1` with `Element` removed.

## Examples

```erlang
1> S = ordsets:from_list([a,b,c]).
2> ordsets:del_element(c, S).
[a,b]
3> ordsets:del_element(x, S).
[a,b,c]
```
""".
-spec del_element(Element, Ordset1) -> Ordset2 when
      Element :: term(),
      Ordset1 :: ordset(T),
      Ordset2 :: ordset(T).

del_element(E, [H|Es]) when E > H -> [H|del_element(E, Es)];
del_element(E, [H|_]=Set) when E < H -> Set;
del_element(_E, [_H|Es]) -> Es;			%E == H
del_element(_, []) -> [].

-doc """
Returns the union of `Ordset1` and `Ordset2`.

The union of two sets is a new set that contains all the elements from
both sets, without duplicates.

## Examples

```erlang
1> S0 = ordsets:from_list([a,b,c,d]).
2> S1 = ordsets:from_list([c,d,e,f]).
3> ordsets:union(S0, S1).
[a,b,c,d,e,f]
```
""".
-spec union(Ordset1, Ordset2) -> Ordset3 when
      Ordset1 :: ordset(T1),
      Ordset2 :: ordset(T2),
      Ordset3 :: ordset(T1 | T2).

union([E1|Es1], [E2|_]=Set2) when E1 < E2 ->
    [E1|union(Es1, Set2)];
union([E1|_]=Set1, [E2|Es2]) when E1 > E2 ->
    [E2|union(Es2, Set1)];			% switch arguments!
union([E1|Es1], [_E2|Es2]) ->			%E1 == E2
    [E1|union(Es1, Es2)];
union([], Es2) -> Es2;
union(Es1, []) -> Es1.

-doc """
Returns the union of a list of sets.

The union of multiple sets is a new set that contains all the elements from
all sets, without duplicates.

## Examples

```erlang
1> S0 = ordsets:from_list([a,b,c,d]).
2> S1 = ordsets:from_list([d,e,f]).
3> S2 = ordsets:from_list([q,r]).
4> Sets = [S0, S1, S2].
5> ordsets:union(Sets).
[a,b,c,d,e,f,q,r]
```
""".
-spec union(OrdsetList) -> Ordset when
      OrdsetList :: [ordset(T)],
      Ordset :: ordset(T).

union(OrdsetList) ->
    lists:umerge(OrdsetList).

-doc """
Returns the intersection of `Ordset1` and `Ordset2`.

The intersection of two sets is a new set that contains only the
elements that are present in both sets.

## Examples

```erlang
1> S0 = ordsets:from_list([a,b,c,d]).
2> S1 = ordsets:from_list([c,d,e,f]).
3> S2 = ordsets:from_list([q,r]).
4> ordsets:intersection(S0, S1).
[c,d]
5> ordsets:intersection(S1, S2).
[]
```
""".
-spec intersection(Ordset1, Ordset2) -> Ordset3 when
      Ordset1 :: ordset(_),
      Ordset2 :: ordset(_),
      Ordset3 :: ordset(_).

intersection([E1|Es1], [E2|_]=Set2) when E1 < E2 ->
    intersection(Es1, Set2);
intersection([E1|_]=Set1, [E2|Es2]) when E1 > E2 ->
    intersection(Es2, Set1);			% switch arguments!
intersection([E1|Es1], [_E2|Es2]) ->		%E1 == E2
    [E1|intersection(Es1, Es2)];
intersection([], _) ->
    [];
intersection(_, []) ->
    [].

-doc """
Returns the intersection of the non-empty list of sets.

The intersection of multiple sets is a new set that contains only the
elements that are present in all sets.

## Examples

```erlang
1> S0 = ordsets:from_list([a,b,c,d]).
2> S1 = ordsets:from_list([d,e,f]).
3> S2 = ordsets:from_list([q,r]).
4> Sets = [S0, S1, S2].
5> ordsets:intersection([S0, S1, S2]).
[]
6> ordsets:intersection([S0, S1]).
[d]
7> ordsets:intersection([]).
** exception error: no function clause matching ordsets:intersection([])
```
""".
-spec intersection(OrdsetList) -> Ordset when
      OrdsetList :: [ordset(_),...],
      Ordset :: ordset(_).

intersection([S1,S2|Ss]) ->
    intersection1(intersection(S1, S2), Ss);
intersection([S]) -> S.

intersection1(S1, [S2|Ss]) ->
    intersection1(intersection(S1, S2), Ss);
intersection1(S1, []) -> S1.

-doc """
Returns `true` if `Ordset1` and `Ordset2` are disjoint; otherwise,
returns `false`.

Two sets are disjoint if they have no elements in common.

This function is equivalent to `ordsets:is_empty(ordsets:intersection(Ordset1, Ordset2))`, but faster.

## Examples

```erlang
1> S0 = ordsets:from_list([a,b,c,d]).
2> S1 = ordsets:from_list([d,e,f]).
3> S2 = ordsets:from_list([q,r]).
4> ordsets:is_disjoint(S0, S1).
false
5> ordsets:is_disjoint(S1, S2).
true
```
""".
-spec is_disjoint(Ordset1, Ordset2) -> boolean() when
      Ordset1 :: ordset(_),
      Ordset2 :: ordset(_).

is_disjoint([E1|Es1], [E2|_]=Set2) when E1 < E2 ->
    is_disjoint(Es1, Set2);
is_disjoint([E1|_]=Set1, [E2|Es2]) when E1 > E2 ->
    is_disjoint(Es2, Set1);			% switch arguments!
is_disjoint([_E1|_Es1], [_E2|_Es2]) ->		%E1 == E2
    false;
is_disjoint([], _) ->
    true;
is_disjoint(_, []) ->
    true.

-doc """
Returns a new ordered set containing the elements of `Ordset1`
that are not elements in `Ordset2`.

## Examples

```erlang
1> S0 = ordsets:from_list([a,b,c,d]).
2> S1 = ordsets:from_list([c,d,e,f]).
3> ordsets:subtract(S0, S1).
[a,b]
4> ordsets:subtract(S1, S0).
[e,f]
```
""".
-spec subtract(Ordset1, Ordset2) -> Ordset3 when
      Ordset1 :: ordset(_),
      Ordset2 :: ordset(_),
      Ordset3 :: ordset(_).

subtract([E1|Es1], [E2|_]=Set2) when E1 < E2 ->
    [E1|subtract(Es1, Set2)];
subtract([E1|_]=Set1, [E2|Es2]) when E1 > E2 ->
    subtract(Set1, Es2);
subtract([_E1|Es1], [_E2|Es2]) ->		%E1 == E2
    subtract(Es1, Es2);
subtract([], _) -> [];
subtract(Es1, []) -> Es1.

-doc """
Returns `true` when every element of `Ordset1` is also a member of `Ordset2`;
otherwise, returns `false`.

## Examples

```erlang
1> S0 = ordsets:from_list([a,b,c,d]).
2> S1 = ordsets:from_list([c,d]).
3> ordsets:is_subset(S1, S0).
true
4> ordsets:is_subset(S0, S1).
false
5> ordsets:is_subset(S0, S0).
true
```
""".
-spec is_subset(Ordset1, Ordset2) -> boolean() when
      Ordset1 :: ordset(_),
      Ordset2 :: ordset(_).

is_subset([E1|_], [E2|_]) when E1 < E2 ->	%E1 not in Set2
    false;
is_subset([E1|_]=Set1, [E2|Es2]) when E1 > E2 ->
    is_subset(Set1, Es2);
is_subset([_E1|Es1], [_E2|Es2]) ->		%E1 == E2
    is_subset(Es1, Es2);
is_subset([], _) -> true;
is_subset(_, []) -> false.

-doc """
Folds `Function` over every element in `Ordset` and returns the final value of
the accumulator.

## Examples

```erlang
1> S = ordsets:from_list([1,2,3,4]).
2> Plus = fun erlang:'+'/2.
3> ordsets:fold(Plus, 0, S).
10
```
""".
-spec fold(Function, Acc0, Ordset) -> Acc1 when
      Function :: fun((Element :: T, AccIn :: term()) -> AccOut :: term()),
      Ordset :: ordset(T),
      Acc0 :: term(),
      Acc1 :: term().

fold(F, Acc, Set) ->
    lists:foldl(F, Acc, Set).

-doc """
Filters elements in `Ordset1` using predicate function `Pred`.

## Examples

```erlang
1> S = ordsets:from_list([1,2,3,4,5,6,7]).
2> IsEven = fun(N) -> N rem 2 =:= 0 end.
3> ordsets:filter(IsEven, S).
[2,4,6]
```
""".
-spec filter(Pred, Ordset1) -> Ordset2 when
      Pred :: fun((Element :: T) -> boolean()),
      Ordset1 :: ordset(T),
      Ordset2 :: ordset(T).

filter(F, Set) ->
    lists:filter(F, Set).

-doc """
Maps elements in `Ordset1` with mapping function `Fun`.

## Examples

```erlang
1> S = ordsets:from_list([1,2,3,4,5,6,7]).
2> F = fun(N) -> N div 2 end.
3> ordsets:map(F, S).
[0,1,2,3]
```
""".
-doc(#{since => <<"OTP 27.0">>}).
-spec map(Fun, Ordset1) -> Ordset2 when
    Fun :: fun((Element1 :: T1) -> Element2 :: T2),
    Ordset1 :: ordset(T1),
    Ordset2 :: ordset(T2).

map(F, Set) ->
    from_list(lists:map(F, Set)).

-doc """
Calls `Fun(Elem)` for each `Elem` of `Ordset1` to update or remove
elements from `Ordset1`.

`Fun/1` must return either a Boolean or a tuple `{true, Value}`. The
function returns the set of elements for which `Fun` returns a new
value, with `true` being equivalent to `{true, Elem}`.

`ordsets:filtermap/2` behaves as if it were defined as follows:

```erlang
filtermap(Fun, Ordset1) ->
    ordsets:from_list(lists:filtermap(Fun, ordsets:to_list(Ordset1))).
```

## Examples

```erlang
1> S = ordsets:from_list([2,4,5,6,8,9]).
2> F = fun(X) ->
           case X rem 2 of
               0 -> {true, X div 2};
               1 -> false
           end
        end.
3> ordsets:filtermap(F, S).
[1,2,3,4]
```
""".
-doc(#{since => <<"OTP 27.0">>}).
-spec filtermap(Fun, Ordset1) -> Ordset2 when
      Fun :: fun((Element1 :: T1) -> boolean() | {true, Element2 :: T2}),
      Ordset1 :: ordset(T1),
      Ordset2 :: ordset(T1 | T2).

filtermap(F, Set) ->
    from_list(lists:filtermap(F, Set)).

%% ---------------------------------------------------------------------------
%% The A.9 runner appended to the verbatim `ordsets` source above.
%%
%% `Reference` is the `.beam` the SYSTEM compiler produced for this identical source: the
%% harness's own `erlc` output for the case module (and, once the oracle side has run, the
%% oracle's in-process artifact of the same source — the option difference between the two is
%% invisible in the `Code` chunk, which is what `deterministic` is about). It is read BEFORE
%% the E0 compile below, whose `{outdir, Dir}` overwrites that very file.
compile_self() ->
    Dir = filename:dirname(code:which(?MODULE)),
    Source = filename:join(Dir, "selfhost_stdlib.erl"),
    Beam = filename:join(Dir, "selfhost_stdlib.beam"),
    Reference = code_chunk(Beam),
    {ok, ?MODULE} = compile:file(Source, [no_spawn_compiler_process, deterministic, {outdir, Dir}]),
    Produced = code_chunk(Beam),
    {ok, Bytes} = file:read_file(Beam),
    %% 1 iff the produced object code is byte-identical to the system compiler's artifact AND
    %% the produced `.beam` is non-empty and carries no occurrence of the case directory (the
    %% `deterministic` stripping that makes the artifact host-independent).
    case {Produced =:= Reference, byte_size(Bytes) > 0,
          binary:match(Bytes, list_to_binary(Dir))} of
        {true, true, nomatch} -> 1;
        _ -> 2
    end.

code_chunk(Beam) ->
    {ok, {?MODULE, [{"Code", Code}]}} = beam_lib:chunks(Beam, ["Code"]),
    Code.
"#;


/// The embedded module of the **dependent pair** case: two sources staged and compiled in one
/// in-process run, then linked and called through.
///
/// Shape rules, all load-bearing:
///
/// * the pair is staged **write-first** (the two sources go through the runtime's file-server
///   write path before any compile, exactly like the sibling routes);
/// * both modules are compiled **in one run, dependency order** (`selfhost_pair_a` first): the
///   dependency is a real cross-module call (`selfhost_pair_b:twice/1` calls
///   `selfhost_pair_a:add/2`), so the pair is only usable if both artifacts exist and agree;
/// * `no_spawn_compiler_process` keeps both compiles in the calling process and
///   `deterministic` makes each artifact path-independent;
/// * the link round trip is `code:add_patha(Dir)` + `code:load_file/1` for **both** modules
///   followed by a call *through* the freshly compiled objects — `1` iff that call returns the
///   value only the linked pair can produce, so a runtime that merely wrote two files cannot
///   fake it. (Both are floor-served code-server entries; `code:load_binary/3` is *not* used —
///   see the module docs of this group's parent: this runtime serves the path-loading route,
///   which is the one the self-hosted build uses.)
const SELFHOST_PAIR_SOURCE: &str = r#"-module(selfhost_pair).
-export([compile_self/0]).

%% Compile a two-module dependency pair IN-PROCESS, in dependency order, and then LINK it:
%% put the case directory on the code path, load both freshly compiled modules through the
%% code server, and call through them (b's `twice/1` calls a's `add/2`).
compile_self() ->
    Dir = filename:dirname(code:which(?MODULE)),
    A = filename:join(Dir, "selfhost_pair_a.erl"),
    B = filename:join(Dir, "selfhost_pair_b.erl"),
    ok = file:write_file(A, a_source()),
    ok = file:write_file(B, b_source()),
    {ok, selfhost_pair_a} = compile:file(A, [no_spawn_compiler_process, deterministic, {outdir, Dir}]),
    {ok, selfhost_pair_b} = compile:file(B, [no_spawn_compiler_process, deterministic, {outdir, Dir}]),
    true = code:add_patha(Dir),
    {module, selfhost_pair_a} = code:load_file(selfhost_pair_a),
    {module, selfhost_pair_b} = code:load_file(selfhost_pair_b),
    case selfhost_pair_b:twice(21) of
        42 -> 1;
        _ -> 2
    end.

%% The dependency: `b` adds a number to itself by calling into `a`.
a_source() ->
    <<"-module(selfhost_pair_a).\n-export([add/2]).\nadd(X, Y) -> X + Y.\n">>.

b_source() ->
    <<"-module(selfhost_pair_b).\n-export([twice/1]).\ntwice(X) -> selfhost_pair_a:add(X, X).\n">>.
"#;

/// The embedded module of the **code-path / load-state** case.
///
/// It pins the code-server surface the compile-and-load route stands on, as a classified
/// shape (never a path):
///
/// * `code:ensure_loaded(lists)` first — this runtime's boot set is smaller than the oracle's,
///   so a load state must be *made* explicit before it is asserted (the sibling `boot`/
///   `boot_extra` cases do the same);
/// * `code:is_loaded/1` reports a `{file, File}` pair for that loaded module (classified by the
///   file's **basename**, so no host path ever reaches the observable);
/// * `code:add_patha(Dir)` the case directory becomes the **head** of `code:get_path/0` — the
///   hook the pair case's `code:load_file/1` relies on;
/// * `code:all_loaded/0` contains that module **as a member** (the loaded-set shape);
/// * `init:fetch_loaded/0` — the boot script's own "initially loaded" manifest (the runtime's
///   `init` floor answers it) — is a list.
///
/// `1` iff every one of those holds, a distinct integer otherwise.
const SELFHOST_PATH_SOURCE: &str = r#"-module(selfhost_path).
-export([compile_self/0]).

%% The A.9 code-path / load-state shapes: make the load explicit, classify the loaded-module
%% pair by basename, prepend the case directory to the code path, and confirm the loaded-set
%% membership plus the boot script's initially-loaded manifest.
compile_self() ->
    Dir = filename:dirname(code:which(?MODULE)),
    {module, lists} = code:ensure_loaded(lists),
    Loaded = code:is_loaded(lists),
    true = code:add_patha(Dir),
    Head = hd(code:get_path()),
    Member = lists:keymember(lists, 1, code:all_loaded()),
    classify(Loaded, Head =:= Dir, Member, init:fetch_loaded()).

classify({file, File}, true, true, Boot) when is_list(Boot) ->
    case filename:basename(File) of
        "lists.beam" -> 1;
        _ -> 2
    end;
classify(_, _, _, _) -> 3.
"#;

/// The embedded module of the **file-borne documentation** case: a module whose
/// `-moduledoc` names a **sibling file**, plus the runner below.
///
/// Shape rules, all load-bearing:
///
/// * the `-moduledoc({file, "moduledoc.txt"})` attribute is the whole point: `epp` cannot take
///   the doc text from the token stream, so it must **read the file** —
///   `scan_filedoc_content/5` (`lib/stdlib/src/epp.erl:1431-1463`) opens it with
///   `[read, binary]`, `file:read_file_info/1`s it and `file:read/2`s exactly `size/1` bytes.
///   With epp being an io-device client, that walk is the io-service `read_handle_info`
///   request — the one file-service surface this case exists to pin;
/// * the doc file is **staged through the runtime's own write path first** (`file:write_file/2`,
///   like every sibling route), and the path is the case dir, which is where epp resolves a
///   relative doc file (the head of its include path is the dir of the file being compiled);
/// * the observable is the produced object code's **`Docs` chunk**, not the compile's return
///   value: `epp` turns an unreadable doc file into a **warning** plus an empty doc
///   (`:1458-1463`), so `{ok, Mod}` alone would be satisfied by a runtime that never served the
///   read. `1` iff the chunk's module doc is byte-identical to the text this run wrote;
/// * the doc text is plain ASCII with no leading or trailing whitespace, so the compiler's own
///   normalisation (`string:trim/1`, `lib/compiler/src/beam_doc.erl:519`) is an identity on it
///   and the chunk's value is exactly the file's bytes.
const SELFHOST_DOCFILE_SOURCE: &str = r#"-module(selfhost_docfile).
-moduledoc({file, "moduledoc.txt"}).
-export([compile_self/0]).

%% A.9: this module's documentation lives in a SIBLING FILE, so `epp` reads it through the
%% file server (`file:read_file_info/1` + `file:read/2`, the file_io_service
%% `read_handle_info` request) while `compile:file/2` parses and compiles this very module
%% IN-PROCESS. `1` iff the produced object code's `Docs` chunk carries exactly the text this
%% run staged into that file, `2` otherwise.
compile_self() ->
    Dir = filename:dirname(code:which(?MODULE)),
    Source = filename:join(Dir, "selfhost_docfile.erl"),
    DocFile = filename:join(Dir, "moduledoc.txt"),
    Doc = list_to_binary(doc_text()),
    ok = file:write_file(DocFile, Doc),
    {ok, ?MODULE} = compile:file(Source, [no_spawn_compiler_process, deterministic, {outdir, Dir}]),
    Beam = filename:join(Dir, "selfhost_docfile.beam"),
    case module_doc(Beam) =:= Doc of
        true -> 1;
        false -> 2
    end.

%% The module documentation this run stages into the sibling file. Plain ASCII, and no leading
%% or trailing whitespace, so the compiler's `string:trim/1` normalisation is an identity on it
%% and the `Docs` chunk's module doc is byte-identical to this file's content.
doc_text() ->
    "This module's documentation lives in a sibling file.\n"
    "\n"
    "The -moduledoc({file, ...}) attribute makes epp read that file through the file\n"
    "server, so the compiler's Docs chunk carries the text this run staged.".

%% The module doc the produced object code carries, read back from its `Docs` chunk. The chunk
%% is a compressed `docs_v1` term whose module-doc field is a bare binary before OTP 27 and a
%% language-keyed map from OTP 27 on, so both shapes are accepted: the golden is the text
%% equality, never a doc-format detail of the oracle's release.
module_doc(Beam) ->
    {ok, {?MODULE, [{"Docs", Docs}]}} = beam_lib:chunks(Beam, ["Docs"]),
    case binary_to_term(Docs) of
        {docs_v1, _Anno, _Mod, _Format, Doc, _Meta, _Docs} when is_binary(Doc) ->
            Doc;
        {docs_v1, _Anno, _Mod, _Format, Doc, _Meta, _Docs} when is_map(Doc) ->
            maps:get(<<"en">>, Doc, <<>>)
    end.
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
                      report whether two such compilations yield byte-identical Code chunks",
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
            stdout: Some("1"),
            exit_code: 0,
        },
    }
}

/// The real-stdlib-module case: a verbatim `ordsets` compiled in-process, its object code
/// compared byte-for-byte against the system compiler's artifact for the identical source.
///
/// The observable is deliberately **not** an `erlang:md5/1` of the produced `.beam` (nor its
/// byte length): both are compiler-release artifacts, and this corpus' oracle is whichever
/// OTP release is installed (`$ERTS_ORACLE` / `erl` on `$PATH`) — a hash pinned here would be
/// reproduced by one release and not by another, i.e. corpus drift. What is pinned instead is
/// the *byte-level equality* itself: the runtime's produced object code must be identical to
/// the object code the system compiler produced from the identical source on the very same
/// host. The compiler's `Code` chunk is exactly the path/time/option-independent (and hence
/// comparable) part of a `.beam`, which is why the comparison is on it; the third clause of
/// the classification pins that the produced artifact is itself path-free
/// (`deterministic`'s stripping), i.e. that the bytes really are host-independent.
fn stdlib_case() -> Case {
    Case {
        id: "selfhost/in_process_compile_real_stdlib_module_byte_identical",
        description: "the A.9 real-stdlib shape: compile a verbatim stdlib module source \
                      (lib/stdlib/src/ordsets.erl) in-process \
                      (no_spawn_compiler_process, deterministic, {outdir, Dir}) and report \
                      whether the produced .beam's Code chunk is byte-identical to the system \
                      compiler's artifact for the identical source",
        contracts: &[Contract::BeamAsm, Contract::Build],
        kind: CaseKind::Module(ModuleRun {
            source: SELFHOST_STDLIB_SOURCE,
            module: "selfhost_stdlib",
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

/// The dependent-pair case: two modules compiled in one in-process run (dependency order),
/// then linked through the code path and called through.
fn pair_case() -> Case {
    Case {
        id: "selfhost/in_process_compile_dependent_pair",
        description: "the A.9 dependent-pair shape: compile two modules the second of which \
                      calls the first, in one in-process run (dependency order), then \
                      code:add_patha/1 + code:load_file/1 both and call through the pair",
        contracts: &[Contract::BeamAsm, Contract::Build],
        kind: CaseKind::Module(ModuleRun {
            source: SELFHOST_PAIR_SOURCE,
            module: "selfhost_pair",
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

/// The code-path / load-state case: `code:add_patha/1` plus the loaded-module surface the
/// compile-and-load route consumes, reduced to a classified shape.
fn path_case() -> Case {
    Case {
        id: "selfhost/code_patha_and_loaded_module_shapes",
        description: "the A.9 code-path / load-state shapes: code:add_patha/1 puts the case \
                      dir at the head of code:get_path/0, and code:ensure_loaded/1 + \
                      code:is_loaded/1 + code:all_loaded/1 + init:fetch_loaded/0 report the \
                      loaded-module surface (classified by basename)",
        contracts: &[Contract::BeamAsm, Contract::Build],
        kind: CaseKind::Module(ModuleRun {
            source: SELFHOST_PATH_SOURCE,
            module: "selfhost_path",
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

/// The file-borne documentation case: a module whose `-moduledoc` names a sibling file,
/// recompiled in-process after that file is staged, with the produced `Docs` chunk's module
/// doc compared against the staged text.
///
/// The observable is deliberately the **doc content**, never the compile's return value: `epp`
/// downgrades an unreadable doc file to a warning and compiles an empty module doc, so
/// `{ok, Mod}` would be reproduced by a runtime that never served the doc-file read. It is also
/// never a hash of the produced `.beam` (a compiler-release artifact — see the module docs):
/// the text of the doc file is release-independent, so the oracle reproduces the golden on any
/// OTP release.
fn docfile_case() -> Case {
    Case {
        id: "selfhost/compile_file_of_a_module_whose_moduledoc_lives_in_a_file",
        description: "the A.9 file-borne documentation shape: recompile in-process \
                      (no_spawn_compiler_process, deterministic, {outdir, Dir}) a module whose \
                      source carries -moduledoc({file, \"moduledoc.txt\"}) after staging that \
                      doc file through the file server, and report whether the produced .beam's \
                      Docs chunk carries exactly that text as the module doc",
        contracts: &[Contract::BeamAsm, Contract::Build],
        kind: CaseKind::Module(ModuleRun {
            source: SELFHOST_DOCFILE_SOURCE,
            module: "selfhost_docfile",
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

/// The **large real module** case: an in-process `compile:file/2` of the repository's own
/// `lib/stdlib/src/timer.erl` (1017 source lines), projected onto the **export table** of the
/// object image it produced.
///
/// Shape rules, all load-bearing:
///
/// * the source is addressed **repo-relatively** (`lib/stdlib/src/timer.erl`): the harness
///   spawns both runtimes with the repository root as their working directory, so the same
///   expression compiles the same tree-relative file on both sides;
/// * `[binary, no_spawn_compiler_process, deterministic]` keeps the compile in the calling
///   process and returns the object image *without* writing a file — the case needs no outdir,
///   so it can be an expression case (both runtimes get identical argv) and no temp path can
///   leak into the observable;
/// * `deterministic` also makes the image host-independent, which the third clause pins: the
///   produced `.beam` must carry **no** occurrence of the source path;
/// * the compile's own failure is **not** caught, so a runtime that cannot host the route
///   raises its typed capability gap (the CLI then exits `3`) instead of answering a
///   plausible-looking value.
///
/// # Why this case is the group's only expression case
///
/// The strongest observable the group has — byte-identical object code against the **system
/// compiler's** artifact for the identical source ([`stdlib_case`]) — needs the case
/// directory's `erlc` output as its reference, which only the module-run argv shape provides.
/// That shape is out of reach here on cost: the very same in-process compile driven through
/// the runtime's `run` entry point (the module-run argv) costs **≈ 43 s** on the Rust runtime
/// against **≈ 2.3 s** through `-eval` (measured at the commit that landed this case; the
/// `ordsets` sibling shows the same penalty on a smaller source — 7.6 s versus 0.8 s), which a
/// gate that boots both runtimes for every CLI case cannot carry. Landing the case on the
/// `-eval` route keeps it at ≈ 2.3 s per Rust-side run.
///
/// # Why the observable is the export table, never a hash of the artifact
///
/// A raw `erlang:md5/1` of the produced image — and equally `beam_lib:md5/1`, which covers the
/// `AtU8`/`Code`/`StrT`/`ImpT`/`ExpT`/`FunT`/`LitT`/`Meta` chunks, i.e. the object code itself
/// — is a **compiler-release** artifact, so pinning either as a golden would be corpus drift
/// (see the module docs). The **export table** is a projection of the produced object code onto
/// something the *source* determines: the module's `-export` attributes plus the compiler's
/// own `module_info/0,1`, so the oracle reproduces it on any OTP release. It is still read back
/// out of the **produced** `.beam` (`beam_lib:chunks(Bin, [exports])`), so it is a property of
/// the compiled image rather than of the source text.
///
/// The remaining clauses pin what every sibling route pins about a `deterministic` artifact —
/// that `compile:file/2` returned the module's object image, that `beam_lib:md5/1` of it is
/// the documented 16-byte value, and that the image carries no source path — so a runtime that
/// merely returned a truncated or host-dependent binary cannot answer the golden.
const SELFHOST_TIMER_EXPR: &str = r#"begin {ok, timer, Bin} = compile:file("lib/stdlib/src/timer.erl", [binary, no_spawn_compiler_process, deterministic]), {ok, {timer, [{exports, Exports}]}} = beam_lib:chunks(Bin, [exports]), {ok, {timer, Md5}} = beam_lib:md5(Bin), true = byte_size(Md5) =:= 16, nomatch = binary:match(Bin, <<"lib/stdlib/src">>), lists:sort(Exports) end"#;

/// The large-real-module case: `lib/stdlib/src/timer.erl` compiled in-process, its produced
/// object image's export table pinned.
///
/// The golden is the **oracle's own stdout line** (a `Probe::Value` rendering of the sorted
/// export list), so it is reproduced by any OTP release — the value is what the *source*
/// declares, not what one compiler release emits (see [`SELFHOST_TIMER_EXPR`]).
fn timer_case() -> Case {
    Case {
        id: "selfhost/compile_file_of_a_module_folding_over_an_ordered_iterator",
        description: "the A.9 large-real-module shape: compile lib/stdlib/src/timer.erl (1017 \
                      source lines) in-process (binary, no_spawn_compiler_process, \
                      deterministic) and report the produced object image's export table, after \
                      pinning that compile:file/2 returned the image, that beam_lib:md5/1 of it \
                      is the documented 16-byte value, and that the image carries no source path",
        contracts: &[Contract::BeamAsm, Contract::Build],
        kind: CaseKind::Eval {
            expr: SELFHOST_TIMER_EXPR,
            extra_argv: &[],
            probe: Probe::Value,
        },
        golden: Golden {
            stdout: Some(
                "ok [{apply_after,2},{apply_after,3},{apply_after,4},{apply_interval,2},\
                 {apply_interval,3},{apply_interval,4},{apply_repeatedly,2},\
                 {apply_repeatedly,3},{apply_repeatedly,4},{cancel,1},{code_change,3},\
                 {exit_after,2},{exit_after,3},{handle_call,3},{handle_cast,2},{handle_info,2},\
                 {hms,3},{hours,1},{init,1},{kill_after,1},{kill_after,2},{minutes,1},\
                 {module_info,0},{module_info,1},{now_diff,2},{seconds,1},{send_after,2},\
                 {send_after,3},{send_interval,2},{send_interval,3},{sleep,1},{start,0},\
                 {start_link,0},{tc,1},{tc,2},{tc,3},{tc,4},{terminate,2}]",
            ),
            exit_code: 0,
        },
    }
}

/// The **same source twice** relation of the file-free (`[binary]`) compile form.
///
/// [`timer_case`] projects *one* compile onto its export table; this case pins that the route is
/// **reproducible**: the same source compiled twice in the same run yields the same bytes, the
/// same `Code` chunk and the same `beam_lib:md5/1` image, and each artifact is path-free. Every
/// clause is either an equality between two artifacts *this run* produced or a documented shape,
/// so no hash **value** is pinned and the oracle reproduces the golden on any OTP release.
///
/// The subject is `lib/stdlib/src/dets_sup.erl` (42 source lines — the smallest stdlib module
/// carrying a `-behaviour` attribute), addressed repo-relatively because the harness spawns both
/// runtimes with the repository root as the working directory. Its export table is source-derived
/// (`-export([start_link/0, init/1])` plus the compiler's `module_info/0,1`), so it is pinned
/// exactly; the artifact's own bytes never are.
const SELFHOST_TWO_RUN_EXPR: &str = r#"begin {ok, ModA, First} = compile:file("lib/stdlib/src/dets_sup.erl", [binary, no_spawn_compiler_process, deterministic]), {ok, ModB, Second} = compile:file("lib/stdlib/src/dets_sup.erl", [binary, no_spawn_compiler_process, deterministic]), {ok, {ModA, [{"Code", ChunkA}]}} = beam_lib:chunks(First, ["Code"]), {ok, {ModB, [{"Code", ChunkB}]}} = beam_lib:chunks(Second, ["Code"]), {ok, {ModA, MdA}} = beam_lib:md5(First), {ok, {ModB, MdB}} = beam_lib:md5(Second), {ok, {ModA, [{exports, Exports}]}} = beam_lib:chunks(First, [exports]), (ModA =:= dets_sup) andalso (ModB =:= ModA) andalso (First =:= Second) andalso (ChunkA =:= ChunkB) andalso (MdA =:= MdB) andalso (byte_size(First) > 0) andalso (byte_size(MdA) =:= 16) andalso (Exports =:= lists:sort(Exports)) andalso (Exports =:= lists:sort([{init, 1}, {module_info, 0}, {module_info, 1}, {start_link, 0}])) andalso (binary:match(First, <<"lib/stdlib/src">>) =:= nomatch) end"#;

/// The **artifact-versus-source** relation: the produced object code records the source's own
/// attributes, and it declares the chunks a loadable `.beam` requires.
///
/// Two relations, both release-independent:
///
/// * `-behaviour(supervisor)` is a **source** attribute, so the `attributes` chunk of the
///   produced image must carry `[{behaviour,[supervisor]}]` — an artifact that dropped or
///   mangled the attribute chunk cannot reproduce it. The compiler-generated `vsn` entry is
///   pinned by its **presence** only (it is an md5-derived integer, i.e. exactly the
///   compiler-release artifact the module docs forbid pinning);
/// * `beam_lib:info/1` of the produced image must declare every chunk the loader needs to
///   resolve code (`AtU8`, `Code`, `StrT`, `ImpT`, `ExpT`) — the artifact is a real object file,
///   not merely a binary of the right size. The chunk *sizes*/offsets are never pinned.
const SELFHOST_ATTRIBUTES_EXPR: &str = r#"begin {ok, Cm, Bin} = compile:file("lib/stdlib/src/dets_sup.erl", [binary, no_spawn_compiler_process, deterministic]), {ok, {dets_sup, [{attributes, Attrs}]}} = beam_lib:chunks(Bin, [attributes]), Info = beam_lib:info(Bin), {chunks, Chs} = lists:keyfind(chunks, 1, Info), Names = [N || {N, _Pos, _Size} <- Chs], (Cm =:= dets_sup) andalso (proplists:get_value(behaviour, Attrs) =:= [supervisor]) andalso (proplists:is_defined(vsn, Attrs)) andalso (proplists:get_value(vsn, Attrs) =/= undefined) andalso (lists:all(fun(Name) -> lists:member(Name, Names) end, ["AtU8", "Code", "StrT", "ImpT", "ExpT"])) end"#;

/// The **sibling-header** relation: a source that `-include`s a header from its **own**
/// directory only compiles if `epp` really read that header through the file server.
///
/// `lib/ssh/src/ssh_bits.erl` (69 source lines) carries `-include("ssh.hrl")`, which `epp`
/// resolves relative to the compiled file's directory and reads as a plain file (with `epp` an
/// io-device client, that walk is the io-service read the [`docfile_case`] route pins for a
/// `-moduledoc({file, …})`). A missing include is a hard compile **error**, so the `{ok, …}`
/// match below cannot be satisfied by a runtime that skipped the read.
///
/// The observable is again a *relation*: the produced image's `Code` chunk, `beam_lib:md5/1`
/// shape and **export table** (`-export([mpint/1, name_list/1])` + `-export([random/1])` plus
/// `module_info/0,1`) — all source-derived — and that the image carries no source path. The
/// `ssh` **application** is not claimed here ([`Contract::OtpApps`] is deliberately absent from
/// this group): only the source text's include chain is exercised.
const SELFHOST_SIBLING_HEADER_EXPR: &str = r#"begin {ok, Cm, Bin} = compile:file("lib/ssh/src/ssh_bits.erl", [binary, no_spawn_compiler_process, deterministic]), {ok, {ssh_bits, [{exports, Ex}]}} = beam_lib:chunks(Bin, [exports]), {ok, {ssh_bits, Md}} = beam_lib:md5(Bin), (Cm =:= ssh_bits) andalso (byte_size(Bin) > 0) andalso (byte_size(Md) =:= 16) andalso (Ex =:= lists:sort([{mpint, 1}, {name_list, 1}, {random, 1}, {module_info, 0}, {module_info, 1}])) andalso (binary:match(Bin, <<"lib/ssh/src">>) =:= nomatch) end"#;

/// The **route-independence** relation: the two ways to host the compile produce the *same*
/// object code.
///
/// [`case`] pins the E0 route (`no_spawn_compiler_process`, the compile runs in the calling
/// process) and [`sched_case`] the default one (`compile:file/2`'s own compiler process, the
/// caller parked in the receive). Each compares its artifact only against itself, so neither
/// can tell whether the two routes agree. This case does: the same source is compiled once
/// through each route with the `binary` (file-free) form, and the two artifacts' `Code` chunks
/// and export tables must be identical — the relation `no_spawn_compiler_process` only changes
/// *where* the work runs, never *what* it produces. Both sides of every equality are produced by
/// the same run, so no compiler release can change the golden.
///
/// The subject is the 42-line `lib/stdlib/src/dets_sup.erl` (see [`SELFHOST_TWO_RUN_EXPR`]).
const SELFHOST_TWO_ROUTE_EXPR: &str = r#"begin {ok, dets_sup, BinE0} = compile:file("lib/stdlib/src/dets_sup.erl", [binary, no_spawn_compiler_process, deterministic]), {ok, dets_sup, BinSched} = compile:file("lib/stdlib/src/dets_sup.erl", [binary, deterministic]), {ok, {dets_sup, [{"Code", CodeE0}]}} = beam_lib:chunks(BinE0, ["Code"]), {ok, {dets_sup, [{"Code", CodeSched}]}} = beam_lib:chunks(BinSched, ["Code"]), {ok, {dets_sup, [{exports, ExE0}]}} = beam_lib:chunks(BinE0, [exports]), {ok, {dets_sup, [{exports, ExSched}]}} = beam_lib:chunks(BinSched, [exports]), (byte_size(BinE0) > 0) andalso (byte_size(BinSched) > 0) andalso (CodeE0 =:= CodeSched) andalso (ExE0 =:= ExSched) end"#;

/// The **extra include directory** relation: `{i, Dir}` is what makes an out-of-directory
/// header reachable, and `epp` must consult that path list through the file server.
///
/// `lib/xmerl/src/xmerl_xpath_lib.erl` (59 source lines) carries `-include("xmerl.hrl")`, and
/// that header lives in the **application's** `include/` directory, not next to the source — so
/// the compile succeeds **only** if the `{i, "lib/xmerl/include"}` option is honoured. A runtime
/// that ignored the option (or could not read a file outside the module's own directory) fails
/// the compile, which the `{ok, …}` match turns into a raise, never into a plausible value.
///
/// The observable is the same source-derived projection as its siblings: the produced image's
/// export table (`-export([eval/3])` plus `module_info/0,1`), a non-empty image with a 16-byte
/// `beam_lib:md5/1` image, and no source path in the artifact.
const SELFHOST_INCLUDE_DIR_EXPR: &str = r#"begin {ok, Cm, Bin} = compile:file("lib/xmerl/src/xmerl_xpath_lib.erl", [binary, no_spawn_compiler_process, deterministic, {i, "lib/xmerl/include"}]), {ok, {xmerl_xpath_lib, [{exports, Ex}]}} = beam_lib:chunks(Bin, [exports]), (Cm =:= xmerl_xpath_lib) andalso (byte_size(Bin) > 0) andalso (Ex =:= lists:sort([{eval, 3}, {module_info, 0}, {module_info, 1}])) andalso (binary:match(Bin, <<"lib/xmerl/src">>) =:= nomatch) end"#;

/// The **`-include_lib`** relation: the header is located through the installation's own code
/// path (`code:lib_dir(App)`), not through a relative path.
///
/// `lib/ftp/src/ftp_progress.erl` (139 source lines) carries
/// `-include_lib("kernel/include/file.hrl")`, so `epp` must first resolve `kernel`'s lib
/// directory and then read the header from there — the code-path/lib-dir resolution the
/// self-hosted build stands on, exercised through the compiler rather than by asking
/// `code:lib_dir/1` directly. A missing header is a hard compile error, so success proves the
/// whole chain ran.
///
/// The export table (`-export([start_link/1, report/2, stop/1])` + `-export([init/1])` plus
/// `module_info/0,1`) is source-derived and pinned exactly; the artifact's bytes are not, beyond
/// being non-empty and carrying no source path.
const SELFHOST_INCLUDE_LIB_EXPR: &str = r#"begin {ok, Cm, Bin} = compile:file("lib/ftp/src/ftp_progress.erl", [binary, no_spawn_compiler_process, deterministic]), {ok, {ftp_progress, [{exports, Ex}]}} = beam_lib:chunks(Bin, [exports]), (Cm =:= ftp_progress) andalso (byte_size(Bin) > 0) andalso (Ex =:= lists:sort([{init, 1}, {module_info, 0}, {module_info, 1}, {report, 2}, {start_link, 1}, {stop, 1}])) andalso (binary:match(Bin, <<"lib/ftp/src">>) =:= nomatch) end"#;

/// The two-run determinism case: the same source compiled twice, its two artifacts compared.
fn two_run_case() -> Case {
    Case {
        id: "selfhost/binary_form_compile_is_byte_identical_across_two_runs",
        description: "the A.9 reproducibility relation: compile lib/stdlib/src/dets_sup.erl \
                      twice in one run with the file-free form (binary, \
                      no_spawn_compiler_process, deterministic) and report whether the two \
                      artifacts' bytes, Code chunks and beam_lib:md5/1 images are identical, the \
                      image is path-free, and the export table is the source's own",
        contracts: &[Contract::BeamAsm, Contract::Build],
        kind: CaseKind::Eval {
            expr: SELFHOST_TWO_RUN_EXPR,
            extra_argv: &[],
            probe: Probe::Value,
        },
        golden: Golden {
            stdout: Some("ok true"),
            exit_code: 0,
        },
    }
}

/// The artifact-versus-source case: the produced image's attributes and chunk set.
fn attributes_case() -> Case {
    Case {
        id: "selfhost/binary_form_compile_pins_the_source_attributes_and_chunk_set",
        description: "the A.9 artifact relation: compile lib/stdlib/src/dets_sup.erl in-process \
                      and report whether the produced image's attributes chunk carries the \
                      source's -behaviour(supervisor) attribute (plus the compiler's vsn entry) \
                      and whether beam_lib:info/1 declares the chunks a loadable .beam requires",
        contracts: &[Contract::BeamAsm, Contract::Build],
        kind: CaseKind::Eval {
            expr: SELFHOST_ATTRIBUTES_EXPR,
            extra_argv: &[],
            probe: Probe::Value,
        },
        golden: Golden {
            stdout: Some("ok true"),
            exit_code: 0,
        },
    }
}

/// The sibling-header case: a source whose `-include` names a header from its own directory.
fn sibling_header_case() -> Case {
    Case {
        id: "selfhost/in_process_compile_reads_a_sibling_header_and_pins_the_export_table",
        description: "the A.9 sibling-header shape: compile lib/ssh/src/ssh_bits.erl in-process \
                      (its -include(\"ssh.hrl\") resolves next to the source) and report whether \
                      the produced image carries a 16-byte beam_lib:md5/1 image, is path-free, \
                      and declares exactly the source's export table",
        contracts: &[Contract::BeamAsm, Contract::Build],
        kind: CaseKind::Eval {
            expr: SELFHOST_SIBLING_HEADER_EXPR,
            extra_argv: &[],
            probe: Probe::Value,
        },
        golden: Golden {
            stdout: Some("ok true"),
            exit_code: 0,
        },
    }
}

/// The route-independence case: the E0 and the scheduled-process routes compared against each
/// other.
fn two_route_case() -> Case {
    Case {
        id: "selfhost/in_process_and_scheduled_compiles_produce_the_same_object_code",
        description: "the A.9 route-independence relation: compile lib/stdlib/src/dets_sup.erl \
                      through both routes (no_spawn_compiler_process in the calling process, and \
                      compile:file/2's own compiler process) with the file-free form and report \
                      whether the two artifacts' Code chunks and export tables are identical",
        contracts: &[Contract::BeamAsm, Contract::Build],
        kind: CaseKind::Eval {
            expr: SELFHOST_TWO_ROUTE_EXPR,
            extra_argv: &[],
            probe: Probe::Value,
        },
        golden: Golden {
            stdout: Some("ok true"),
            exit_code: 0,
        },
    }
}

/// The extra-include-directory case: `{i, Dir}` as the load-bearing option.
fn include_dir_case() -> Case {
    Case {
        id: "selfhost/in_process_compile_honours_an_extra_include_directory",
        description: "the A.9 include-path relation: compile lib/xmerl/src/xmerl_xpath_lib.erl, \
                      whose header lives in the application's include/ directory, with the \
                      {i, \"lib/xmerl/include\"} option and report whether the produced image \
                      declares exactly the source's export table and carries no source path",
        contracts: &[Contract::BeamAsm, Contract::Build],
        kind: CaseKind::Eval {
            expr: SELFHOST_INCLUDE_DIR_EXPR,
            extra_argv: &[],
            probe: Probe::Value,
        },
        golden: Golden {
            stdout: Some("ok true"),
            exit_code: 0,
        },
    }
}

/// The `-include_lib` case: the header located through the installation's code path.
fn include_lib_case() -> Case {
    Case {
        id: "selfhost/in_process_compile_resolves_an_include_lib_header",
        description: "the A.9 include-lib relation: compile lib/ftp/src/ftp_progress.erl, whose \
                      -include_lib(\"kernel/include/file.hrl\") is located through \
                      code:lib_dir/1, and report whether the produced image declares exactly the \
                      source's export table and carries no source path",
        contracts: &[Contract::BeamAsm, Contract::Build],
        kind: CaseKind::Eval {
            expr: SELFHOST_INCLUDE_LIB_EXPR,
            extra_argv: &[],
            probe: Probe::Value,
        },
        golden: Golden {
            stdout: Some("ok true"),
            exit_code: 0,
        },
    }
}

/// The A.9 self-hosted-build group: the six in-process compile / link shapes plus the
/// large-real-module expression case and the six relation cases the expression route adds
/// (two-run determinism, artifact-versus-source attributes and chunk set, sibling header,
/// route independence, an extra include directory, and `-include_lib`).
pub(crate) fn group() -> Vec<Case> {
    vec![
        case(),
        sched_case(),
        stdlib_case(),
        pair_case(),
        path_case(),
        docfile_case(),
        timer_case(),
        two_run_case(),
        attributes_case(),
        sibling_header_case(),
        two_route_case(),
        include_dir_case(),
        include_lib_case(),
    ]
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
    fn the_group_is_the_six_routes_and_the_seven_expression_cases() {
        let cases = group();
        assert_eq!(
            cases.len(),
            13,
            "the group is the two compile:file/2 routes plus the stdlib, pair, code-path, \
             file-borne-documentation, large-real-module and six file-free relation cases"
        );
        assert_eq!(
            cases[0].id,
            "selfhost/in_process_compile_file_no_spawn_deterministic"
        );
        assert_eq!(
            cases[1].id,
            "selfhost/scheduled_process_compile_file_deterministic"
        );
        assert_eq!(
            cases[2].id,
            "selfhost/in_process_compile_real_stdlib_module_byte_identical"
        );
        assert_eq!(cases[3].id, "selfhost/in_process_compile_dependent_pair");
        assert_eq!(cases[4].id, "selfhost/code_patha_and_loaded_module_shapes");
        assert_eq!(
            cases[5].id,
            "selfhost/compile_file_of_a_module_whose_moduledoc_lives_in_a_file"
        );
        assert_eq!(
            cases[6].id,
            "selfhost/compile_file_of_a_module_folding_over_an_ordered_iterator"
        );
        assert_eq!(
            cases[7].id,
            "selfhost/binary_form_compile_is_byte_identical_across_two_runs"
        );
        assert_eq!(
            cases[8].id,
            "selfhost/binary_form_compile_pins_the_source_attributes_and_chunk_set"
        );
        assert_eq!(
            cases[9].id,
            "selfhost/in_process_compile_reads_a_sibling_header_and_pins_the_export_table"
        );
        assert_eq!(
            cases[10].id,
            "selfhost/in_process_and_scheduled_compiles_produce_the_same_object_code"
        );
        assert_eq!(
            cases[11].id,
            "selfhost/in_process_compile_honours_an_extra_include_directory"
        );
        assert_eq!(
            cases[12].id,
            "selfhost/in_process_compile_resolves_an_include_lib_header"
        );
        for case in &cases {
            assert_eq!(case.contracts, &[Contract::BeamAsm, Contract::Build]);
            assert_eq!(case.golden.exit_code, 0);
        }
        // The six route cases are module-run cases with integer goldens; the seven remaining
        // cases are expression cases whose golden is a relation (`ok true`) — the large-real
        // module compile pins its `~w` export list, the six relation cases a boolean.
        let (routes, exprs) = cases.split_at(6);
        for case in routes {
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
        // Each case runs its **own** module, so the temp dirs never collide.
        assert_eq!(run_of(&routes[0]).module, "selfhost");
        assert_eq!(run_of(&routes[1]).module, "selfhost_sched");
        assert_eq!(run_of(&routes[2]).module, "selfhost_stdlib");
        assert_eq!(run_of(&routes[3]).module, "selfhost_pair");
        assert_eq!(run_of(&routes[4]).module, "selfhost_path");
        assert_eq!(run_of(&routes[5]).module, "selfhost_docfile");
        for case in routes {
            assert_eq!(case.golden.stdout, Some("1"));
        }
        let CaseKind::Eval {
            expr,
            extra_argv,
            probe,
        } = &exprs[0].kind
        else {
            panic!("case {} must stay an expression case", exprs[0].id);
        };
        assert_eq!(*extra_argv, &[] as &[&str], "no CLI flag is pinned");
        assert_eq!(*probe, Probe::Value, "the golden is the `~w` export list");
        assert!(
            expr.starts_with("begin {ok, timer, Bin} = compile:file(\"lib/stdlib/src/timer.erl\""),
            "the subject is the repo-relative stdlib source, in-process and file-free"
        );
        assert!(!expr.contains("catch"), "the raise must not be caught");
        // The staged routes are write-first: the runtime's file-service write precedes the
        // compile it feeds (the stdlib case stages nothing — the harness wrote its source).
        for (index, write_needle, compile_needle) in [
            (0usize, "file:write_file(Staged", "compile:file(Source"),
            (1, "file:write_file(Staged", "compile:file(Source"),
            (3, "file:write_file(A, a_source())", "compile:file(A,"),
            (3, "file:write_file(B, b_source())", "compile:file(B,"),
            (5, "file:write_file(DocFile, Doc)", "compile:file(Source"),
        ] {
            let source = run_of(&routes[index]).source;
            let write = source.find(write_needle).expect("staging write");
            let compile = source.find(compile_needle).expect("compile");
            assert!(
                write < compile,
                "case {}: the route is staged write-first",
                routes[index].id
            );
        }
        // The compile's own failure must escape in every case: the runtime's typed capability
        // gap (CLI exit 3) is the only honest signal for a runtime that cannot host the route.
        for case in routes {
            assert!(
                !run_of(case).source.contains("catch"),
                "case {}: the raise must not be caught",
                case.id
            );
        }
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
    fn the_scheduled_shape_drops_no_spawn_and_compares_two_deterministic_object_codes() {
        let cases = group();
        let run = run_of(&cases[1]);
        for needle in [
            "-module(selfhost_sched).",
            "code:which(?MODULE)",
            "file:write_file(Staged, Src)",
            "compile:file(Source, [deterministic, {outdir, Dir}])",
            "beam_lib:chunks(Beam, [\"Code\"])",
            "First =:= Second",
        ] {
            assert!(run.source.contains(needle), "the route needs {needle:?}");
        }
        // The observable is a release-independent equality of two compilations, never a
        // compiler-version-dependent bytecode hash.
        assert!(
            !run.source.contains("phash2"),
            "the golden must not embed a compiler-version-dependent hash"
        );
        // The shape is the *default* `compile:file/2`: without the option the compiler runs in
        // its own process (the `erl -noshell -s compile …` / `erlc` shape) — that spawn is the
        // whole difference from the E0 case.
        assert!(
            !run.source.contains("[no_spawn_compiler_process"),
            "the default shape must not keep the compile in the calling process"
        );
        // The staging write must precede the compile (see the E0 case), and each compile must
        // be observed through the object code it wrote.
        let write = run.source.find("file:write_file(Staged").expect("staging");
        let compile = run.source.find("compile:file(Source").expect("compile");
        let chunks = run.source.find("beam_lib:chunks").expect("code chunk");
        assert!(write < compile && compile < chunks, "staged write-first, then read back");
        // The compile's own failure must escape (see the E0 case).
        assert!(!run.source.contains("catch"), "the raise must not be caught");
    }

    /// The real-stdlib case: the subject is a verbatim stdlib module, the reference is the
    /// system compiler's artifact of the identical source, and the observable is a byte-level
    /// equality of the two `Code` chunks — never a compiler-version-dependent hash.
    #[test]
    fn the_stdlib_case_compiles_a_verbatim_stdlib_module_and_compares_object_code() {
        let cases = group();
        let run = run_of(&cases[2]);
        let source = run.source;
        // The subject really is the stdlib module's own code (not a toy reimplementation).
        for needle in [
            "-module(selfhost_stdlib).",
            "-export_type([ordset/1]).",
            "-export([compile_self/0]).",
            "from_list(List) ->",
            "add_element(E, [H|Es]) when E > H -> [H|add_element(E, Es)];",
        ] {
            assert!(source.contains(needle), "the subject needs {needle:?}");
        }
        assert!(
            !source.contains("-module(ordsets)."),
            "the module attribute is renamed so code:which(?MODULE) names the case dir"
        );
        // The subject's prefix is the stdlib source, verbatim: 560 lines ending with the
        // module's own last function, plus the one added `-export([compile_self/0]).` line.
        let last = "    from_list(lists:filtermap(F, Set)).";
        let end = source.find(last).expect("the stdlib module's last function") + last.len();
        let stdlib = &source[..end];
        assert_eq!(
            stdlib.matches('\n').count(),
            560,
            "the subject is the verbatim stdlib source (lib/stdlib/src/ordsets.erl, 559 lines) \
             plus the one added export"
        );
        assert!(
            source.trim_end().ends_with("Code."),
            "the runner's last helper is the subject's tail"
        );
        // The E0 options and the read-reference-before-compile ordering.
        for needle in [
            "code:which(?MODULE)",
            "compile:file(Source, [no_spawn_compiler_process, deterministic, {outdir, Dir}])",
            "beam_lib:chunks(Beam, [\"Code\"])",
        ] {
            assert!(source.contains(needle), "the route needs {needle:?}");
        }
        let reference = source.find("Reference = code_chunk(Beam)").expect("reference");
        let compile = source.find("compile:file(Source").expect("compile");
        let produced = source.find("Produced = code_chunk(Beam)").expect("produced");
        assert!(
            reference < compile && compile < produced,
            "the system compiler's artifact is read before the compile overwrites it"
        );
        assert!(
            source.contains("Produced =:= Reference"),
            "the observable is the object-code equality itself"
        );
        assert!(
            source.contains("binary:match(Bytes, list_to_binary(Dir))"),
            "the produced artifact must be pinned path-free, so the bytes are host-independent"
        );
        // The loaded artifact must never be reduced to a raw hash: that would pin the compiler
        // release into the golden (the oracle release is not fixed across hosts).
        assert!(
            !source.contains("erlang:md5") && !source.contains("phash2"),
            "the golden must not embed a compiler-version-dependent hash"
        );
        assert!(!source.contains("catch"), "the raise must not be caught");
    }

    /// The dependent-pair case: two staged sources, one in-process run in dependency order,
    /// and a link round trip that calls *through* the pair.
    #[test]
    fn the_pair_case_compiles_a_dependency_pair_and_links_it_through_the_code_path() {
        let cases = group();
        let run = run_of(&cases[3]);
        let source = run.source;
        for needle in [
            "-module(selfhost_pair).",
            "file:write_file(A, a_source())",
            "file:write_file(B, b_source())",
            "-module(selfhost_pair_a).",
            "-module(selfhost_pair_b).",
            "selfhost_pair_a:add(X, X)",
            "compile:file(A, [no_spawn_compiler_process, deterministic, {outdir, Dir}])",
            "compile:file(B, [no_spawn_compiler_process, deterministic, {outdir, Dir}])",
            "true = code:add_patha(Dir)",
            "{module, selfhost_pair_a} = code:load_file(selfhost_pair_a)",
            "{module, selfhost_pair_b} = code:load_file(selfhost_pair_b)",
            "selfhost_pair_b:twice(21)",
        ] {
            assert!(source.contains(needle), "the route needs {needle:?}");
        }
        // The dependency must be real and the order must be the dependency order: `b` calls
        // into `a`, and `a` is written and compiled first.
        let write_a = source.find("file:write_file(A, a_source())").expect("stage a");
        let write_b = source.find("file:write_file(B, b_source())").expect("stage b");
        let compile_a = source.find("compile:file(A,").expect("compile a");
        let compile_b = source.find("compile:file(B,").expect("compile b");
        let load_a = source.find("code:load_file(selfhost_pair_a)").expect("load a");
        let load_b = source.find("code:load_file(selfhost_pair_b)").expect("load b");
        assert!(
            write_a < write_b && compile_a < compile_b && load_a < load_b,
            "the pair is staged, compiled and loaded in dependency order"
        );
        assert!(
            compile_b < load_a,
            "both compiles happen in one in-process run, before the link round trip"
        );
        // The link round trip is the path-loading route (`code:add_patha/1` +
        // `code:load_file/1`), never the dynamic `code:load_binary/3` route.
        assert!(
            !source.contains("load_binary"),
            "the link round trip must use the path-loading route"
        );
        assert!(!source.contains("catch"), "the raise must not be caught");
    }

    /// The code-path / load-state case: the load is made explicit first, the loaded-module
    /// pair is classified by basename, and the path head is the case dir.
    #[test]
    fn the_path_case_pins_the_code_path_and_loaded_module_shapes() {
        let cases = group();
        let run = run_of(&cases[4]);
        let source = run.source;
        for needle in [
            "-module(selfhost_path).",
            "code:which(?MODULE)",
            "{module, lists} = code:ensure_loaded(lists)",
            "code:is_loaded(lists)",
            "true = code:add_patha(Dir)",
            "hd(code:get_path())",
            "lists:keymember(lists, 1, code:all_loaded())",
            "init:fetch_loaded()",
            "filename:basename(File)",
            "\"lists.beam\"",
        ] {
            assert!(source.contains(needle), "the route needs {needle:?}");
        }
        // The load state must be *made* explicit before it is asserted: this runtime's boot
        // set is smaller than the oracle's (the sibling boot cases' rule).
        let ensure = source.find("code:ensure_loaded(lists)").expect("ensure_loaded");
        let is_loaded = source.find("code:is_loaded(lists)").expect("is_loaded");
        let add_path = source.find("code:add_patha(Dir)").expect("add_patha");
        assert!(
            ensure < is_loaded && is_loaded < add_path,
            "the load is forced first, then the path is prepended"
        );
        // The observable is a classification, never a path: the loaded module's file is
        // reduced to its basename.
        assert!(
            !source.contains("filename:join(Dir, \"lists"),
            "the observable must not name a resolved module path"
        );
        assert!(!source.contains("catch"), "the raise must not be caught");
    }

    /// The file-borne documentation case: the module's own `-moduledoc` names a sibling file,
    /// the doc file is staged write-first, and the observable is the produced `Docs` chunk's
    /// module doc compared against the staged text — never a hash, and never the compile's
    /// return value (an unreadable doc file is only a warning).
    #[test]
    fn the_docfile_case_reads_the_moduledoc_file_and_pins_the_docs_chunk() {
        let cases = group();
        let run = run_of(&cases[5]);
        let source = run.source;
        for needle in [
            "-module(selfhost_docfile).",
            "-moduledoc({file, \"moduledoc.txt\"}).",
            "code:which(?MODULE)",
            "file:write_file(DocFile, Doc)",
            "compile:file(Source, [no_spawn_compiler_process, deterministic, {outdir, Dir}])",
            "beam_lib:chunks(Beam, [\"Docs\"])",
            "module_doc(Beam) =:= Doc",
        ] {
            assert!(source.contains(needle), "the route needs {needle:?}");
        }
        // The doc file must be staged through the runtime's own write path *before* the compile
        // that makes `epp` read it back.
        let write = source.find("file:write_file(DocFile, Doc)").expect("staging");
        let compile = source.find("compile:file(Source").expect("compile");
        let docs = source.find("beam_lib:chunks(Beam, [\"Docs\"])").expect("docs chunk");
        assert!(write < compile && compile < docs, "staged write-first, then read back");
        // The doc-file read is what `epp` drives; `file:read_file_info/2` on an **io device** is
        // the `read_handle_info` request, so the route must reach it through `file:read/2`'s
        // io-service walk, never by reading the file some other way (i.e. the doc file must be
        // named, not inlined).
        assert!(
            !source.contains("-moduledoc \""),
            "the module doc must come from the file, not from an inline string"
        );
        // The doc text is the observable, so it must survive the compiler's `string:trim/1`
        // unchanged: plain ASCII with no trailing newline on the last literal segment.
        assert!(source.is_ascii(), "the staged doc text must be plain ASCII");
        assert!(
            source.contains("carries the text this run staged.\"."),
            "the doc text's last literal segment must end without a trailing newline"
        );
        // No compiler-version-dependent hash may reach the golden (see the module docs).
        assert!(
            !source.contains("erlang:md5") && !source.contains("phash2"),
            "the golden must not embed a compiler-version-dependent hash"
        );
        assert!(!source.contains("catch"), "the raise must not be caught");
    }

    /// The large-real-module case: the subject is the repository's own
    /// `lib/stdlib/src/timer.erl` addressed **repo-relatively**, the route is the file-free
    /// `binary` form of the E0 compile, and the observable is the produced image's **export
    /// table** — never a compiler-release hash.
    #[test]
    fn the_timer_case_compiles_the_repo_stdlib_source_and_pins_the_export_table() {
        let cases = group();
        // The large-real-module case is the **first** expression case, i.e. the one right after
        // the six module-run routes.
        let case = &cases[6];
        assert_eq!(
            case.id,
            "selfhost/compile_file_of_a_module_folding_over_an_ordered_iterator"
        );
        let CaseKind::Eval {
            expr,
            extra_argv,
            probe,
        } = &case.kind
        else {
            panic!("case {} is not an expression case", case.id);
        };
        for needle in [
            "compile:file(\"lib/stdlib/src/timer.erl\"",
            "[binary, no_spawn_compiler_process, deterministic]",
            "beam_lib:chunks(Bin, [exports])",
            "beam_lib:md5(Bin)",
            "binary:match(Bin, <<\"lib/stdlib/src\">>)",
            "lists:sort(Exports)",
        ] {
            assert!(expr.contains(needle), "the route needs {needle:?}");
        }
        assert_eq!(*extra_argv, &[] as &[&str], "no CLI flag is pinned");
        // The compile must be the file-free `binary` form: an expression case hands both
        // runtimes identical argv, so there is no case directory to write an outdir into — and
        // no temp path can reach the observable either.
        assert!(
            !expr.contains("{outdir") && !expr.contains("code:which"),
            "the expression route writes no artifact and owns no case directory"
        );
        // The observable is the source-derived export table, never a hash of the artifact: both
        // `erlang:md5/1` and the *value* of `beam_lib:md5/1` are compiler-release artifacts (see
        // the module docs), so only the documented 16-byte *shape* of the latter is pinned.
        assert!(
            !expr.contains("erlang:md5") && !expr.contains("phash2"),
            "the golden must not embed a compiler-version-dependent hash"
        );
        assert!(
            expr.contains("byte_size(Md5) =:= 16") && !expr.contains("Md5 =:="),
            "the md5 is pinned by its shape, never by its value"
        );
        assert_eq!(*probe, Probe::Value, "the golden is the `~w` export list");
        assert!(!expr.contains("catch"), "the raise must not be caught");
        // The golden is the oracle's own `~w` line: no path, no pid/port/ref, no clock value.
        let golden = case.golden.stdout.expect("the case pins its stdout");
        assert!(
            golden.starts_with("ok [{apply_after,2},"),
            "the golden is the `~w` of the sorted export list"
        );
        assert!(
            golden.ends_with("{terminate,2}]"),
            "the export list is complete"
        );
        assert!(!golden.contains('/'), "the golden must not carry a path");
    }

    /// The expression case at `index`, panicking when it is not an expression case.
    fn eval_of(cases: &[Case], index: usize) -> (&'static str, &'static [&'static str], Probe) {
        let CaseKind::Eval {
            expr,
            extra_argv,
            probe,
        } = &cases[index].kind
        else {
            panic!("case {} is not an expression case", cases[index].id);
        };
        (*expr, *extra_argv, *probe)
    }
    /// The shared guard on the six file-free relation cases: each pins a **relation** of the
    /// produced object code — its golden is `ok true`, the compile is the file-free `[binary]`
    /// form, no compiler-release artifact (`erlang:md5/1`, `phash2/1`, a chunk image, a path, a
    /// clock reading) can reach the observable, and the raise is never caught.
    #[test]
    fn the_relation_cases_pin_relations_never_compiler_release_artifacts() {
        let cases = group();
        for (index, subject) in [
            (7usize, "lib/stdlib/src/dets_sup.erl"),
            (8, "lib/stdlib/src/dets_sup.erl"),
            (9, "lib/ssh/src/ssh_bits.erl"),
            (10, "lib/stdlib/src/dets_sup.erl"),
            (11, "lib/xmerl/src/xmerl_xpath_lib.erl"),
            (12, "lib/ftp/src/ftp_progress.erl"),
        ] {
            let case = &cases[index];
            assert_eq!(case.golden.stdout, Some("ok true"), "case {}", case.id);
            let (expr, extra_argv, probe) = eval_of(&cases, index);
            assert_eq!(extra_argv, &[] as &[&str], "no CLI flag is pinned");
            assert_eq!(probe, Probe::Value, "the golden is a `~w` value");
            assert!(
                expr.contains(&format!("compile:file(\"{subject}\"")),
                "case {}: the subject is the repo-relative source",
                case.id
            );
            assert!(
                expr.contains("[binary, no_spawn_compiler_process, deterministic"),
                "case {}: the route is the file-free in-process form",
                case.id
            );
            // The relation itself: every case is a chain of `=:=`/`andalso` clauses, never a
            // pinned value of the compiler's output.
            assert!(
                expr.contains("andalso"),
                "case {}: the case is a relation",
                case.id
            );
            assert!(
                !expr.contains("erlang:md5") && !expr.contains("phash2"),
                "case {}: no compiler-release hash may reach the golden",
                case.id
            );
            assert!(
                !expr.contains("catch"),
                "case {}: the raise must not be caught",
                case.id
            );
            // An expression case has no case directory, so an outdir could only be a temp path.
            assert!(
                !expr.contains("{outdir") && !expr.contains("code:which"),
                "case {}: the expression route writes no artifact",
                case.id
            );
            // No probe-wrapper binding (`V`, `C`, `R`) may be shadowed by the case's own code.
            for forbidden in [" V", " C", " R"] {
                assert!(
                    !expr.contains(&format!("{forbidden} =")),
                    "case {}: the probe wrapper already binds that variable",
                    case.id
                );
            }
        }
    }

    /// The two-run case: the reproducibility relation, and the source-derived export table it
    /// pins exactly (never the artifact's bytes).
    #[test]
    fn the_two_run_case_compares_two_artifacts_of_the_same_source() {
        let cases = group();
        let (expr, _, _) = eval_of(&cases, 7);
        for needle in [
            "compile:file(\"lib/stdlib/src/dets_sup.erl\", [binary, no_spawn_compiler_process, deterministic])",
            "beam_lib:chunks(First, [\"Code\"])",
            "beam_lib:md5(First)",
            "(First =:= Second)",
            "(ChunkA =:= ChunkB)",
            "(MdA =:= MdB)",
            "(byte_size(MdA) =:= 16)",
            "(Exports =:= lists:sort([{init, 1}, {module_info, 0}, {module_info, 1}, {start_link, 0}]))",
            "(binary:match(First, <<\"lib/stdlib/src\">>) =:= nomatch)",
        ] {
            assert!(expr.contains(needle), "the relation needs {needle:?}");
        }
        // Two compiles of the same source: the case is a comparison, not a single artifact.
        assert_eq!(
            expr.matches("compile:file(").count(),
            2,
            "the case compiles the source twice"
        );
        // No artifact is compared against a literal: only the two runs are compared with each
        // other, so no compiler-release image can reach the golden.
        assert!(
            expr.contains("(MdA =:= MdB)") && !expr.contains("(MdA =:= <<"),
            "the md5 images are only compared with each other, never pinned"
        );
    }

    /// The attributes case: a source attribute (`-behaviour(supervisor)`) read back out of the
    /// produced image, and the loader's chunk set — the compiler's `vsn` value is never pinned.
    #[test]
    fn the_attributes_case_pins_the_source_attribute_and_the_chunk_set() {
        let cases = group();
        let (expr, _, _) = eval_of(&cases, 8);
        for needle in [
            "beam_lib:chunks(Bin, [attributes])",
            "(proplists:get_value(behaviour, Attrs) =:= [supervisor])",
            "(proplists:is_defined(vsn, Attrs))",
            // The `vsn` value is an md5-derived integer: presence only, never the value.
            "(proplists:get_value(vsn, Attrs) =/= undefined)",
            "beam_lib:info(Bin)",
            "[\"AtU8\", \"Code\", \"StrT\", \"ImpT\", \"ExpT\"]",
        ] {
            assert!(expr.contains(needle), "the relation needs {needle:?}");
        }
        assert!(
            !expr.contains("vsn, Attrs) =:= ["),
            "the compiler's own vsn value must never be pinned"
        );
    }

    /// The sibling-header case: a source whose `-include` names a header next to it, so a
    /// runtime that cannot serve epp's read cannot produce the artifact at all.
    #[test]
    fn the_sibling_header_case_compiles_the_ssh_source_and_pins_its_export_table() {
        let cases = group();
        let (expr, _, _) = eval_of(&cases, 9);
        for needle in [
            "compile:file(\"lib/ssh/src/ssh_bits.erl\"",
            "(Ex =:= lists:sort([{mpint, 1}, {name_list, 1}, {random, 1}, {module_info, 0}, {module_info, 1}]))",
            "(byte_size(Md) =:= 16)",
            "(binary:match(Bin, <<\"lib/ssh/src\">>) =:= nomatch)",
        ] {
            assert!(expr.contains(needle), "the relation needs {needle:?}");
        }
        // The subject really carries a same-directory `-include`, which is the header read this
        // case exists to pin.
        let source = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../lib/ssh/src/ssh_bits.erl"
        ))
        .expect("the ssh source is in the tree");
        assert!(
            source.contains("-include(\"ssh.hrl\")."),
            "the case's subject must include a sibling header"
        );
    }

    /// The route-independence case: the E0 and the scheduled-process routes' artifacts compared
    /// against each other, so a difference between the two compile routes cannot hide.
    #[test]
    fn the_two_route_case_compares_the_e0_and_scheduled_artifacts() {
        let cases = group();
        let (expr, _, _) = eval_of(&cases, 10);
        for needle in [
            "[binary, no_spawn_compiler_process, deterministic]",
            // The other route: binary form *without* `no_spawn_compiler_process`.
            "compile:file(\"lib/stdlib/src/dets_sup.erl\", [binary, deterministic])",
            "(CodeE0 =:= CodeSched)",
            "(ExE0 =:= ExSched)",
        ] {
            assert!(expr.contains(needle), "the relation needs {needle:?}");
        }
    }

    /// The extra-include-directory case: `{i, Dir}` is load-bearing (the header is not next to
    /// the source), so the option cannot be dropped without failing the compile.
    #[test]
    fn the_include_dir_case_pins_the_load_bearing_include_option() {
        let cases = group();
        let (expr, _, _) = eval_of(&cases, 11);
        for needle in [
            "compile:file(\"lib/xmerl/src/xmerl_xpath_lib.erl\"",
            "{i, \"lib/xmerl/include\"}",
            "(Ex =:= lists:sort([{eval, 3}, {module_info, 0}, {module_info, 1}]))",
        ] {
            assert!(expr.contains(needle), "the relation needs {needle:?}");
        }
        // The relation is a relation only while the header is *not* next to the source: the
        // application's `include/` directory holds it, the `src/` directory does not.
        let root = concat!(env!("CARGO_MANIFEST_DIR"), "/../../..");
        assert!(
            std::path::Path::new(&format!("{root}/lib/xmerl/include/xmerl.hrl")).is_file(),
            "the header must live in the application's include dir"
        );
        assert!(
            !std::path::Path::new(&format!("{root}/lib/xmerl/src/xmerl.hrl")).is_file(),
            "the header must NOT live next to the source, or the option proves nothing"
        );
    }

    /// The `-include_lib` case: the header is located through the installation's code path.
    #[test]
    fn the_include_lib_case_pins_the_code_path_header_resolution() {
        let cases = group();
        let (expr, _, _) = eval_of(&cases, 12);
        for needle in [
            "compile:file(\"lib/ftp/src/ftp_progress.erl\"",
            "(Ex =:= lists:sort([{init, 1}, {module_info, 0}, {module_info, 1}, {report, 2}, {start_link, 1}, {stop, 1}]))",
        ] {
            assert!(expr.contains(needle), "the relation needs {needle:?}");
        }
        // No `{i, …}`: the header must be found through `-include_lib` (the code path), not
        // through a relative include directory.
        assert!(
            !expr.contains("{i,"),
            "the include_lib route must not also pass an include directory"
        );
        let source = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../lib/ftp/src/ftp_progress.erl"
        ))
        .expect("the ftp source is in the tree");
        assert!(
            source.contains("-include_lib(\"kernel/include/file.hrl\")."),
            "the case's subject must resolve its header through include_lib"
        );
    }
}
