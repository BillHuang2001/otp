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
//! a **dependent pair** compiled and linked in one run, and the **code-path / load-state**
//! shapes the route stands on.
//!
//! `../../../CONTEXT.md` (its "A.9 — what an in-process compile actually needs" section) is
//! the design source: OTP's `compile:file/2` path reaches only `kernel` + `stdlib` (`epp` — an
//! io-device-only client — plus `erl_features`, `erl_scan`/`erl_parse`/`erl_lint`/
//! `erl_expand_records`, the `v3_core` → `beam_*` passes and `save_binary`'s write). The E0
//! route keeps the whole compile in the **calling** process; the default shape spawns
//! `compile:file/2`'s compiler process, and the caller parks in the receive that waits for it.
//!
//! This group is the **evidence scaffold** for that route: five module-run cases whose
//! goldens are the oracle's own integer lines, so each pins a property of the *produced
//! object code* rather than a mere "it returned".
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
//! produced (`.beam` `Code` chunks), or a shape classified into an integer.
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
//! All five cases **agree** on the Rust runtime: the runtime hosts the E0 route, the
//! scheduled shape, an in-process compile of a real stdlib module, the dependent pair (with
//! the `code:add_patha/1` + `code:load_file/1` link round trip) and the code-path /
//! load-state shapes. Three contracts are in play and exactly two are claimed:
//! [`Contract::BeamAsm`] (a real `.beam` executes and object code is produced) and
//! [`Contract::Build`] (the build/bootstrapping route itself). [`Contract::OtpApps`] is
//! **not** claimed — the routes' whole point is that they reach only `kernel` + `stdlib`, so
//! nothing about them depends on the wider application surface.

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

/// The A.9 self-hosted-build group: the five in-process compile / link shapes.
pub(crate) fn group() -> Vec<Case> {
    vec![
        case(),
        sched_case(),
        stdlib_case(),
        pair_case(),
        path_case(),
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
    fn the_group_is_the_five_self_hosted_build_shapes() {
        let cases = group();
        assert_eq!(
            cases.len(),
            5,
            "the group is the two compile:file/2 routes plus the stdlib, pair and code-path cases"
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
        // Each case runs its **own** module, so the temp dirs never collide.
        assert_eq!(run_of(&cases[0]).module, "selfhost");
        assert_eq!(run_of(&cases[1]).module, "selfhost_sched");
        assert_eq!(run_of(&cases[2]).module, "selfhost_stdlib");
        assert_eq!(run_of(&cases[3]).module, "selfhost_pair");
        assert_eq!(run_of(&cases[4]).module, "selfhost_path");
        for case in &cases {
            assert_eq!(case.golden.stdout, Some("1"));
        }
        // The staged routes are write-first: the runtime's file-service write precedes the
        // compile it feeds (the stdlib case stages nothing — the harness wrote its source).
        for (index, write_needle, compile_needle) in [
            (0usize, "file:write_file(Staged", "compile:file(Source"),
            (1, "file:write_file(Staged", "compile:file(Source"),
            (3, "file:write_file(A, a_source())", "compile:file(A,"),
            (3, "file:write_file(B, b_source())", "compile:file(B,"),
        ] {
            let source = run_of(&cases[index]).source;
            let write = source.find(write_needle).expect("staging write");
            let compile = source.find(compile_needle).expect("compile");
            assert!(
                write < compile,
                "case {}: the route is staged write-first",
                cases[index].id
            );
        }
        // The compile's own failure must escape in every case: the runtime's typed capability
        // gap (CLI exit 3) is the only honest signal for a runtime that cannot host the route.
        for case in &cases {
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
}
